//! 角色與權限（RBAC，設計包 `kairos/ia.md` §4；`kairos/API.md` 第 4.4 節）：權限的**集中定義**。
//!
//! - [`Permission`]：主控台 API 的每一種操作。新功能（例如告警的 `alerts:read`／`alerts:write`）只要在
//!   [`Permission`] 加一個變體、在 [`Permission::ALL`] 加一項，並在 [`Role::permissions`] 決定哪些內建角色
//!   擁有它即可；名稱（[`Permission::name`]）是對外的穩定識別字（`/api/v1/me`、403 的 `details.permission`）。
//! - [`Role`]：四個內建角色（管理員、維運者、稽核員、唯讀，`ia.md` §4.1、§4.2）。自訂角色屬於 v1.0 之後。
//! - [`Scope`]：角色指派的範圍——全站或主機群組（`ia.md` §4.3；租戶為 F12 預留，不接受）。
//! - [`Grants`]：一個身分實際擁有的權限：全站的一組，加上每個主機群組的一組。系統層級的操作
//!   （註冊、PKI、設定、使用者、稽核）只看全站的那一組；主機的讀取另外依群組篩選（[`HostAccess`]）。
//! - [`LegacyScope`]：0.4／0.5 建立、還沒有角色的 API token（`read`／`operate`／`admin`）。它們的權限
//!   **凍結**在升級前的集合（[`LegacyScope::permissions`]），之後新增的權限不會自動給它們（升級不得變更
//!   既有 token 的權限）。

use serde_json::{Value, json};
use std::collections::BTreeMap;

/// 權限（主控台 API 的每一種操作）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Permission {
    /// 主機清冊、趨勢、總覽、即時推送（S03～S05）
    HostsRead,
    /// 系統健康（S20）
    HealthRead,
    /// 註冊 token、申請、註冊端點模式的讀取（S12、S13、S21）
    EnrollRead,
    /// 建立、停用註冊 token（只能建立需核准的 token）
    EnrollTokenWrite,
    /// 建立自動核准的 token；自動核准 token 的批次部署模式
    EnrollTokenAuto,
    /// 核准、拒絕申請（S13）
    EnrollDecide,
    /// 核准後會超過授權節點數（本版沒有授權機制，保留給之後的版本）
    EnrollLicenseOverflow,
    /// 切換註冊端點模式、自動暫停（S21）
    EnrollModeWrite,
    /// 憑證、CA、CRL、待簽發佇列的讀取（S14、S15）
    PkiRead,
    /// 撤銷憑證、立即重新產生 CRL
    CertRevoke,
    /// 憑證政策
    CertPolicyWrite,
    /// 允許再次自動續簽（D12）
    GraceOverride,
    /// 離線模式的 CSR 佇列匯出與匯入
    PkiQueue,
    /// 簽發 CA 的建立、安裝、啟用、退役（本版只有 CLI）
    PkiCaWrite,
    /// 設定與來源的讀取（S21）
    SettingsRead,
    /// 使用者、角色指派、工作階段清單的讀取（S16）
    UsersRead,
    /// 建立、停用、刪除使用者，變更角色指派，重設密碼與 MFA（S16）
    UsersWrite,
    /// 結束其他使用者的所有工作階段（S16，D25）
    SessionsRevoke,
    /// API token 清單（S21「整合」）
    ApiTokensRead,
    /// 建立、撤銷 API token
    ApiTokensWrite,
    /// 目錄服務（LDAP／AD）設定與群組對應的讀取（S17，bind 密碼一律遮蔽）
    DirectoryRead,
    /// 目錄服務的群組對應與測試
    DirectoryWrite,
    /// 讀取所有人的稽核紀錄、驗證完整性（S18）
    AuditRead,
    /// 只讀取自己的操作（S18：維運者 `actor=me` 固定）
    AuditReadOwn,
    /// 匯出稽核紀錄（S18）
    AuditExport,
    /// 告警事件的查詢與即時推送（S06、S07：所有角色）
    AlertsRead,
    /// 確認、指派、暫停通知、解決、註解（S06、S07：維運者）
    AlertsWrite,
    /// 告警規則的查詢（S08：唯讀看不到此頁）
    AlertRulesRead,
    /// 建立與編輯自訂規則、啟停規則、調整系統預設規則的通知（S08、S09：維運者）
    AlertRulesWrite,
    /// 調整系統預設規則的條件（S09：只有管理員）
    AlertRulesAdmin,
    /// 靜音與維護時段的查詢（S10：所有角色）
    SilencesRead,
    /// 建立、修改、提前結束靜音與維護時段（S10：維運者）
    SilencesWrite,
    /// 通知管道的清單與狀態（S11：維運者只能看清單與寄送測試；稽核員與唯讀看不到此頁；機密一律遮蔽）
    ChannelsRead,
    /// 對已儲存的管道寄送測試通知（S11：維運者）
    ChannelsTest,
    /// 新增、修改、刪除通知管道，以未儲存的設定測試（管道含機密，S11：只有管理員）
    ChannelsWrite,
}

impl Permission {
    /// 所有權限（新增變體時同步加入；測試會檢查沒有遺漏與名稱不重複）
    pub const ALL: [Permission; 35] = [
        Permission::HostsRead,
        Permission::HealthRead,
        Permission::EnrollRead,
        Permission::EnrollTokenWrite,
        Permission::EnrollTokenAuto,
        Permission::EnrollDecide,
        Permission::EnrollLicenseOverflow,
        Permission::EnrollModeWrite,
        Permission::PkiRead,
        Permission::CertRevoke,
        Permission::CertPolicyWrite,
        Permission::GraceOverride,
        Permission::PkiQueue,
        Permission::PkiCaWrite,
        Permission::SettingsRead,
        Permission::UsersRead,
        Permission::UsersWrite,
        Permission::SessionsRevoke,
        Permission::ApiTokensRead,
        Permission::ApiTokensWrite,
        Permission::DirectoryRead,
        Permission::DirectoryWrite,
        Permission::AuditRead,
        Permission::AuditReadOwn,
        Permission::AuditExport,
        Permission::AlertsRead,
        Permission::AlertsWrite,
        Permission::AlertRulesRead,
        Permission::AlertRulesWrite,
        Permission::AlertRulesAdmin,
        Permission::SilencesRead,
        Permission::SilencesWrite,
        Permission::ChannelsRead,
        Permission::ChannelsTest,
        Permission::ChannelsWrite,
    ];

    /// 對外的名稱（穩定；`<資源>:<動作>`）
    pub fn name(self) -> &'static str {
        match self {
            Permission::HostsRead => "hosts:read",
            Permission::HealthRead => "health:read",
            Permission::EnrollRead => "enroll:read",
            Permission::EnrollTokenWrite => "enroll.tokens:write",
            Permission::EnrollTokenAuto => "enroll.tokens:auto_approve",
            Permission::EnrollDecide => "enroll.requests:decide",
            Permission::EnrollLicenseOverflow => "enroll.license:overflow",
            Permission::EnrollModeWrite => "enroll.mode:write",
            Permission::PkiRead => "pki:read",
            Permission::CertRevoke => "pki.certificates:revoke",
            Permission::CertPolicyWrite => "pki.policy:write",
            Permission::GraceOverride => "pki.grace:override",
            Permission::PkiQueue => "pki.queue:manage",
            Permission::PkiCaWrite => "pki.ca:write",
            Permission::SettingsRead => "settings:read",
            Permission::UsersRead => "users:read",
            Permission::UsersWrite => "users:write",
            Permission::SessionsRevoke => "sessions:revoke",
            Permission::ApiTokensRead => "api_tokens:read",
            Permission::ApiTokensWrite => "api_tokens:write",
            Permission::DirectoryRead => "directory:read",
            Permission::DirectoryWrite => "directory:write",
            Permission::AuditRead => "audit:read",
            Permission::AuditReadOwn => "audit:read_own",
            Permission::AuditExport => "audit:export",
            Permission::AlertsRead => "alerts:read",
            Permission::AlertsWrite => "alerts:write",
            Permission::AlertRulesRead => "alert-rules:read",
            Permission::AlertRulesWrite => "alert-rules:write",
            Permission::AlertRulesAdmin => "alert-rules:admin",
            Permission::SilencesRead => "silences:read",
            Permission::SilencesWrite => "silences:write",
            Permission::ChannelsRead => "channels:read",
            Permission::ChannelsTest => "channels:test",
            Permission::ChannelsWrite => "channels:write",
        }
    }

    #[cfg(test)]
    pub fn parse(name: &str) -> Option<Permission> {
        Permission::ALL.into_iter().find(|p| p.name() == name)
    }

    fn bit(self) -> u128 {
        let i = Permission::ALL
            .iter()
            .position(|p| *p == self)
            .unwrap_or(127);
        1u128 << i
    }

    /// 擁有這個權限的最低內建角色（403 的 `details.required` 給人看的提示）
    pub fn least_role(self) -> Role {
        [Role::Viewer, Role::Auditor, Role::Operator, Role::Admin]
            .into_iter()
            .find(|r| r.permissions().contains(self))
            .unwrap_or(Role::Admin)
    }
}

/// 一組權限
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct PermSet(u128);

impl PermSet {
    pub const EMPTY: PermSet = PermSet(0);

    pub fn of(list: &[Permission]) -> PermSet {
        PermSet(list.iter().fold(0, |acc, p| acc | p.bit()))
    }

    pub fn all() -> PermSet {
        PermSet::of(&Permission::ALL)
    }

    pub fn contains(self, p: Permission) -> bool {
        self.0 & p.bit() != 0
    }

    pub fn union(self, other: PermSet) -> PermSet {
        PermSet(self.0 | other.0)
    }

    pub fn intersect(self, other: PermSet) -> PermSet {
        PermSet(self.0 & other.0)
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// 依 [`Permission::ALL`] 的順序列出
    pub fn list(self) -> Vec<Permission> {
        Permission::ALL
            .into_iter()
            .filter(|p| self.contains(*p))
            .collect()
    }

    pub fn names(self) -> Vec<&'static str> {
        self.list().into_iter().map(Permission::name).collect()
    }
}

/// 內建角色（`ia.md` §4.1）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// 管理員：能做所有事
    Admin,
    /// 維運者：日常監控與處置；不能改系統安全設定與使用者
    Operator,
    /// 稽核員：唯讀全部監控資料，另可讀與匯出稽核日誌；不能做任何變更
    Auditor,
    /// 唯讀：只能看監控資料（總覽、主機、告警事件）
    Viewer,
}

impl Role {
    pub const ALL: [Role; 4] = [Role::Admin, Role::Operator, Role::Auditor, Role::Viewer];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Operator => "operator",
            Role::Auditor => "auditor",
            Role::Viewer => "viewer",
        }
    }

    /// 顯示用的中文名稱（設計包的固定用語）
    pub fn label(self) -> &'static str {
        match self {
            Role::Admin => "管理員",
            Role::Operator => "維運者",
            Role::Auditor => "稽核員",
            Role::Viewer => "唯讀",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        match s.trim().to_ascii_lowercase().as_str() {
            "admin" | "administrator" => Some(Role::Admin),
            "operator" => Some(Role::Operator),
            "auditor" => Some(Role::Auditor),
            "viewer" | "read_only" | "readonly" => Some(Role::Viewer),
            _ => None,
        }
    }

    /// 角色的權限（`ia.md` §4.2 的權限矩陣）。新增權限時在這裡決定哪些角色擁有它
    pub fn permissions(self) -> PermSet {
        use Permission::*;
        match self {
            Role::Admin => PermSet::all(),
            // 維運者：核准註冊、撤銷憑證、立即更新 CRL、離線佇列；唯讀檢視「連線與安全」（D16）；
            // 稽核日誌只看自己的操作；看不到使用者與目錄服務（S16、S17 為「—」）
            Role::Operator => PermSet::of(&[
                HostsRead,
                HealthRead,
                EnrollRead,
                EnrollTokenWrite,
                EnrollDecide,
                PkiRead,
                CertRevoke,
                PkiQueue,
                SettingsRead,
                AuditReadOwn,
                // 告警（S06～S11）：確認、指派、解決、暫停通知；自訂規則；靜音與維護時段；
                // 管道只能看清單與寄送測試（新增、修改、刪除與機密只限管理員）
                AlertsRead,
                AlertsWrite,
                AlertRulesRead,
                AlertRulesWrite,
                SilencesRead,
                SilencesWrite,
                ChannelsRead,
                ChannelsTest,
            ]),
            // 稽核員：所有監控與設定唯讀、使用者與權限清冊、目錄服務（遮蔽 bind 密碼）、稽核日誌與匯出
            Role::Auditor => PermSet::of(&[
                HostsRead,
                HealthRead,
                EnrollRead,
                PkiRead,
                SettingsRead,
                UsersRead,
                ApiTokensRead,
                DirectoryRead,
                AuditRead,
                AuditExport,
                // 告警：事件、規則、靜音唯讀；看不到通知管道（S11 為「—」）
                AlertsRead,
                AlertRulesRead,
                SilencesRead,
            ]),
            // 唯讀：總覽、主機、系統健康（摘要）、告警事件與靜音（唯讀）；看不到稽核、使用者、PKI 細節、
            // 告警規則（S08 為「—」）與通知管道
            Role::Viewer => PermSet::of(&[HostsRead, HealthRead, AlertsRead, SilencesRead]),
        }
    }
}

/// 角色指派的範圍（`ia.md` §4.3）
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// 全站
    Global,
    /// 主機群組（`hosts.host_group`；比對不分大小寫）
    Group(String),
}

/// 群組名稱的長度上限
pub const MAX_GROUP_CHARS: usize = 64;

impl Scope {
    /// 資料庫的 (scope_type, scope_value)
    pub fn to_db(&self) -> (&'static str, String) {
        match self {
            Scope::Global => ("global", String::new()),
            Scope::Group(g) => ("group", g.clone()),
        }
    }

    pub fn from_db(kind: &str, value: &str) -> Option<Scope> {
        match kind {
            "global" => Some(Scope::Global),
            "group" if !value.trim().is_empty() => Some(Scope::Group(value.to_string())),
            _ => None,
        }
    }

    /// 文字形式：`global` 或 `group:<名稱>`
    pub fn as_text(&self) -> String {
        match self {
            Scope::Global => "global".to_string(),
            Scope::Group(g) => format!("group:{g}"),
        }
    }

    /// 解析 `global`、`group:<名稱>`；群組名稱 1～64 個字元、不含控制字元
    pub fn parse(text: &str) -> Result<Scope, String> {
        let t = text.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("global") {
            return Ok(Scope::Global);
        }
        let Some(g) = t.strip_prefix("group:") else {
            return Err(format!(
                "範圍「{t}」無效：global 或 group:<主機群組名稱>（租戶範圍尚未開放）"
            ));
        };
        Scope::group(g)
    }

    pub fn group(name: &str) -> Result<Scope, String> {
        let g = name.trim();
        let n = g.chars().count();
        if n == 0 || n > MAX_GROUP_CHARS || g.chars().any(char::is_control) {
            return Err(format!(
                "主機群組名稱必須是 1～{MAX_GROUP_CHARS} 個字元、不含控制字元"
            ));
        }
        Ok(Scope::Group(g.to_string()))
    }

    pub fn json(&self) -> Value {
        match self {
            Scope::Global => json!({"type": "global"}),
            Scope::Group(g) => json!({"type": "group", "group": g}),
        }
    }
}

/// 一筆角色指派（主體另外保存）
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Assignment {
    pub role: Role,
    pub scope: Scope,
}

impl Assignment {
    pub fn json(&self) -> Value {
        json!({"role": self.role.as_str(), "role_label": self.role.label(), "scope": self.scope.json()})
    }

    /// `admin`、`operator@group:B廠產線` 的文字形式（CLI）
    pub fn as_text(&self) -> String {
        match &self.scope {
            Scope::Global => self.role.as_str().to_string(),
            s => format!("{}@{}", self.role.as_str(), s.as_text()),
        }
    }

    /// 解析 `<角色>` 或 `<角色>@group:<名稱>`
    pub fn parse(text: &str) -> Result<Assignment, String> {
        let (role, scope) = match text.split_once('@') {
            Some((r, s)) => (r, Scope::parse(s)?),
            None => (text, Scope::Global),
        };
        let role = Role::parse(role).ok_or_else(|| {
            format!(
                "角色「{}」無效：admin、operator、auditor 或 viewer",
                role.trim()
            )
        })?;
        Ok(Assignment { role, scope })
    }
}

/// 主機的可見範圍
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAccess {
    /// 全部主機
    All,
    /// 只有這些主機群組（小寫）
    Groups(Vec<String>),
    /// 沒有主機的讀取權限
    None,
}

impl HostAccess {
    /// 這個群組（`hosts.host_group`）中的主機是否可見
    pub fn allows(&self, group: Option<&str>) -> bool {
        match self {
            HostAccess::All => true,
            HostAccess::None => false,
            HostAccess::Groups(gs) => {
                group.is_some_and(|g| gs.iter().any(|x| *x == g.trim().to_lowercase()))
            }
        }
    }
}

/// 一個身分實際擁有的權限
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Grants {
    /// 全站範圍的權限
    pub global: PermSet,
    /// 主機群組範圍的權限（鍵為小寫的群組名稱）
    pub groups: BTreeMap<String, PermSet>,
}

impl Grants {
    pub fn global(set: PermSet) -> Grants {
        Grants {
            global: set,
            groups: BTreeMap::new(),
        }
    }

    /// 由角色指派計算
    pub fn from_assignments<'a>(list: impl IntoIterator<Item = &'a Assignment>) -> Grants {
        let mut g = Grants::default();
        for a in list {
            match &a.scope {
                Scope::Global => g.global = g.global.union(a.role.permissions()),
                Scope::Group(name) => {
                    let e = g.groups.entry(name.trim().to_lowercase()).or_default();
                    // 群組範圍只授予主機的讀取（系統層級的頁面只對全站範圍開放，ia.md §4.3）
                    *e = e.union(
                        a.role
                            .permissions()
                            .intersect(PermSet::of(&[Permission::HostsRead])),
                    );
                }
            }
        }
        g
    }

    /// 兩者都有的權限（擁有者的權限 ∩ token 的角色）
    pub fn intersect(&self, other: &Grants) -> Grants {
        let mut groups = BTreeMap::new();
        for (k, v) in &self.groups {
            // 對方在全站有的權限，在群組中也有
            let o = other
                .groups
                .get(k)
                .copied()
                .unwrap_or_default()
                .union(other.global);
            let s = v.intersect(o);
            if !s.is_empty() {
                groups.insert(k.clone(), s);
            }
        }
        for (k, v) in &other.groups {
            if groups.contains_key(k) {
                continue;
            }
            let s = v.intersect(self.global);
            if !s.is_empty() {
                groups.insert(k.clone(), s);
            }
        }
        Grants {
            global: self.global.intersect(other.global),
            groups,
        }
    }

    /// 全站範圍是否擁有這個權限（系統層級的操作）
    pub fn has(&self, p: Permission) -> bool {
        self.global.contains(p)
    }

    /// 主機的可見範圍
    pub fn host_access(&self) -> HostAccess {
        if self.global.contains(Permission::HostsRead) {
            return HostAccess::All;
        }
        let groups: Vec<String> = self
            .groups
            .iter()
            .filter(|(_, s)| s.contains(Permission::HostsRead))
            .map(|(k, _)| k.clone())
            .collect();
        if groups.is_empty() {
            HostAccess::None
        } else {
            HostAccess::Groups(groups)
        }
    }

    /// `/api/v1/me` 的 `permissions` 與 `group_permissions`
    pub fn json(&self) -> Value {
        json!({
            "permissions": self.global.names(),
            "group_permissions": self.groups.iter()
                .map(|(k, v)| (k.clone(), json!(v.names())))
                .collect::<serde_json::Map<_, _>>(),
        })
    }
}

/// 0.4／0.5 的 API token 範圍（還沒有角色與擁有者的 token）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyScope {
    Read,
    Operate,
    Admin,
}

impl LegacyScope {
    pub fn parse(s: &str) -> Option<LegacyScope> {
        match s.trim() {
            "read" => Some(LegacyScope::Read),
            "operate" => Some(LegacyScope::Operate),
            "admin" => Some(LegacyScope::Admin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LegacyScope::Read => "read",
            LegacyScope::Operate => "operate",
            LegacyScope::Admin => "admin",
        }
    }

    /// 升級前（0.5.0）這個範圍擁有的權限，**凍結**：`read` 是所有 `GET`（主機、總覽、健康、事件、
    /// 註冊、PKI、設定），`operate` 另加日常的註冊與憑證操作，`admin` 另加安全影響等同簽發的操作
    /// （API.md 第 11.1 節）。之後新增的權限（使用者、稽核、目錄服務、告警……）不會加到這裡
    pub fn permissions(self) -> PermSet {
        use Permission::*;
        let read = PermSet::of(&[HostsRead, HealthRead, EnrollRead, PkiRead, SettingsRead]);
        let operate = read.union(PermSet::of(&[
            EnrollTokenWrite,
            EnrollDecide,
            CertRevoke,
            PkiQueue,
        ]));
        match self {
            LegacyScope::Read => read,
            LegacyScope::Operate => operate,
            LegacyScope::Admin => operate.union(PermSet::of(&[
                EnrollTokenAuto,
                EnrollLicenseOverflow,
                EnrollModeWrite,
                CertPolicyWrite,
                GraceOverride,
                PkiCaWrite,
            ])),
        }
    }

    /// 需要這個權限的最低範圍（0.5 的 403 `details.required`）；新權限沒有對應的範圍
    pub fn required_for(p: Permission) -> Option<LegacyScope> {
        [LegacyScope::Read, LegacyScope::Operate, LegacyScope::Admin]
            .into_iter()
            .find(|s| s.permissions().contains(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_permission_is_listed_once_with_a_unique_name() {
        let names: HashSet<&str> = Permission::ALL.iter().map(|p| p.name()).collect();
        assert_eq!(names.len(), Permission::ALL.len());
        assert!(Permission::ALL.len() < 128);
        for p in Permission::ALL {
            assert_eq!(Permission::parse(p.name()), Some(p));
            assert!(p.name().contains(':'), "{}", p.name());
        }
        assert_eq!(PermSet::all().list().len(), Permission::ALL.len());
    }

    #[test]
    fn role_matrix_follows_the_design() {
        use Permission::*;
        let admin = Role::Admin.permissions();
        let op = Role::Operator.permissions();
        let aud = Role::Auditor.permissions();
        let view = Role::Viewer.permissions();
        // 管理員能做所有事
        assert_eq!(admin, PermSet::all());
        // 維運者：核准、撤銷、建立需核准的 token；不能建立自動核准的 token、改模式、政策、CA、使用者
        for p in [
            EnrollDecide,
            CertRevoke,
            EnrollTokenWrite,
            PkiQueue,
            AuditReadOwn,
        ] {
            assert!(op.contains(p), "{p:?}");
        }
        for p in [
            EnrollTokenAuto,
            EnrollModeWrite,
            CertPolicyWrite,
            GraceOverride,
            PkiCaWrite,
            UsersRead,
            UsersWrite,
            SessionsRevoke,
            AuditRead,
            AuditExport,
            DirectoryRead,
            ApiTokensWrite,
        ] {
            assert!(!op.contains(p), "{p:?}");
        }
        // 稽核員：沒有任何變更權限
        for p in PermSet::all().list() {
            let write = p.name().ends_with(":write")
                || matches!(
                    p,
                    EnrollTokenAuto
                        | EnrollDecide
                        | EnrollLicenseOverflow
                        | CertRevoke
                        | GraceOverride
                        | PkiQueue
                        | SessionsRevoke
                        | AlertRulesAdmin
                        | ChannelsTest
                );
            if write {
                assert!(!aud.contains(p), "稽核員不應有 {p:?}");
            }
        }
        assert!(aud.contains(AuditRead) && aud.contains(AuditExport) && aud.contains(UsersRead));
        // 唯讀：看不到稽核、使用者、PKI、註冊；告警事件與靜音唯讀
        assert_eq!(
            view,
            PermSet::of(&[HostsRead, HealthRead, AlertsRead, SilencesRead])
        );
        assert_eq!(UsersWrite.least_role(), Role::Admin);
        assert_eq!(AuditRead.least_role(), Role::Auditor);
        assert_eq!(HostsRead.least_role(), Role::Viewer);
    }

    /// 告警的權限表（設計包 `ia.md` §4.2 S06～S11；`kairos/API.md` 第 13.1 節）
    #[test]
    fn alert_permissions_follow_the_design_matrix() {
        use Permission::*;
        // (權限, 管理員, 維運者, 稽核員, 唯讀)
        let matrix = [
            (AlertsRead, true, true, true, true),
            (AlertsWrite, true, true, false, false),
            (AlertRulesRead, true, true, true, false),
            (AlertRulesWrite, true, true, false, false),
            (AlertRulesAdmin, true, false, false, false),
            (SilencesRead, true, true, true, true),
            (SilencesWrite, true, true, false, false),
            (ChannelsRead, true, true, false, false),
            (ChannelsTest, true, true, false, false),
            (ChannelsWrite, true, false, false, false),
        ];
        for (p, admin, op, aud, view) in matrix {
            assert_eq!(Role::Admin.permissions().contains(p), admin, "admin {p:?}");
            assert_eq!(
                Role::Operator.permissions().contains(p),
                op,
                "operator {p:?}"
            );
            assert_eq!(
                Role::Auditor.permissions().contains(p),
                aud,
                "auditor {p:?}"
            );
            assert_eq!(Role::Viewer.permissions().contains(p), view, "viewer {p:?}");
        }
        assert_eq!(ChannelsWrite.least_role(), Role::Admin);
        assert_eq!(AlertsWrite.least_role(), Role::Operator);
        assert_eq!(AlertsRead.least_role(), Role::Viewer);
        // 群組範圍的指派不授予告警的權限（系統層級；只有全站範圍）
        let g = Grants::from_assignments(&[Assignment::parse("operator@group:a").unwrap()]);
        for (p, ..) in matrix {
            assert!(!g.has(p), "{p:?}");
        }
    }

    #[test]
    fn legacy_scopes_keep_their_pre_upgrade_permissions() {
        use Permission::*;
        // 0.5.0 的 Permission::required_scope：read 為所有讀取，operate／admin 依 API.md 第 11.1 節
        let pre_upgrade: [(Permission, &str); 15] = [
            (HostsRead, "read"),
            (HealthRead, "read"),
            (EnrollRead, "read"),
            (PkiRead, "read"),
            (SettingsRead, "read"),
            (EnrollTokenWrite, "operate"),
            (EnrollDecide, "operate"),
            (CertRevoke, "operate"),
            (PkiQueue, "operate"),
            (EnrollTokenAuto, "admin"),
            (EnrollLicenseOverflow, "admin"),
            (EnrollModeWrite, "admin"),
            (CertPolicyWrite, "admin"),
            (GraceOverride, "admin"),
            (PkiCaWrite, "admin"),
        ];
        for (p, scope) in pre_upgrade {
            assert_eq!(
                LegacyScope::required_for(p).map(LegacyScope::as_str),
                Some(scope),
                "{p:?}"
            );
        }
        // 新增的權限不屬於任何舊範圍（升級後舊 token 不會多出權限）
        for p in [
            UsersRead,
            UsersWrite,
            SessionsRevoke,
            ApiTokensRead,
            ApiTokensWrite,
            DirectoryRead,
            DirectoryWrite,
            AuditRead,
            AuditReadOwn,
            AuditExport,
            AlertsRead,
            AlertsWrite,
            AlertRulesRead,
            AlertRulesWrite,
            AlertRulesAdmin,
            SilencesRead,
            SilencesWrite,
            ChannelsRead,
            ChannelsTest,
            ChannelsWrite,
        ] {
            assert_eq!(LegacyScope::required_for(p), None, "{p:?}");
        }
        assert_eq!(LegacyScope::Admin.permissions().list().len(), 15);
    }

    #[test]
    fn scopes_assignments_and_grants() {
        assert_eq!(Scope::parse("").unwrap(), Scope::Global);
        assert_eq!(
            Scope::parse("group:B廠產線").unwrap(),
            Scope::Group("B廠產線".into())
        );
        assert!(Scope::parse("tenant:x").is_err());
        assert!(Scope::parse("group:").is_err());
        assert!(Scope::parse(&format!("group:{}", "x".repeat(65))).is_err());
        let a = Assignment::parse("operator@group:Line3").unwrap();
        assert_eq!(a.as_text(), "operator@group:Line3");
        assert_eq!(Assignment::parse("ADMIN").unwrap().role, Role::Admin);
        assert!(Assignment::parse("root").is_err());

        // 群組範圍的維運者：只看得到該群組的主機，系統層級的操作一律沒有
        let g = Grants::from_assignments(std::slice::from_ref(&a));
        assert!(!g.has(Permission::EnrollDecide));
        assert!(!g.has(Permission::HostsRead));
        assert_eq!(g.host_access(), HostAccess::Groups(vec!["line3".into()]));
        assert!(g.host_access().allows(Some("LINE3")));
        assert!(!g.host_access().allows(Some("line4")));
        assert!(!g.host_access().allows(None));
        // 全站唯讀＋群組維運者
        let both = Grants::from_assignments(&[
            a,
            Assignment {
                role: Role::Viewer,
                scope: Scope::Global,
            },
        ]);
        assert_eq!(both.host_access(), HostAccess::All);
        assert!(!both.has(Permission::CertRevoke));
        assert_eq!(Grants::default().host_access(), HostAccess::None);

        // 交集：擁有者是管理員、token 角色是唯讀 → 唯讀
        let owner = Grants::global(Role::Admin.permissions());
        let token = Grants::global(Role::Viewer.permissions());
        assert_eq!(owner.intersect(&token).global, Role::Viewer.permissions());
        // 擁有者只有群組範圍，token 是全站管理員 → 只剩群組範圍的主機讀取
        let owner = Grants::from_assignments(&[Assignment::parse("viewer@group:a").unwrap()]);
        let x = owner.intersect(&Grants::global(PermSet::all()));
        assert!(x.global.is_empty());
        assert_eq!(x.host_access(), HostAccess::Groups(vec!["a".into()]));
    }
}
