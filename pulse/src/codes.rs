//! 日誌事件與錯誤代碼（設計包 `pulse/log-ux.md` §4、`design-system/copywriting.md` §3.3）。
//!
//! 有對應事件的日誌行，訊息開頭為「`<event> <code> `」（沒有代碼時為 `-`），順序與 `log-ux.md` §6.1
//! 的欄位相同：
//!
//! ```text
//! 2026-10-13T14:22:16.123+08:00 [WARN] buffer.full PLS-W4202 本機緩衝已滿（待送 50000 筆…），丟掉最舊的資料…
//! ```
//!
//! 規則與搜尋只需要比對 `event` 與 `code`（固定、不翻譯），不依賴訊息文字。`log.format = "json"` 時
//! 這兩欄是 JSON 的 `event`、`code`（由 `kp_obs::logfmt` 從訊息開頭取出）。重試摘要等其餘 `log-ux.md`
//! 的改版見 `ERROR-CODES.md`「待辦」。
//!
//! `win_event_id` 為 Windows 事件記錄的事件識別碼（`log-ux.md` §4 的表）；Pulse 目前沒有寫入
//! Windows 事件記錄（需要安裝程式註冊事件來源），先在這裡定案對照，供日後的事件記錄與測試使用。

/// 固定的日誌事件
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    /// `<領域>.<事件>`，小寫、以點分隔；新增只能附加，不可改名或重用
    pub name: &'static str,
    /// Windows 事件記錄的事件識別碼；只寫檔案的事件為 `None`
    pub win_event_id: Option<u16>,
}

macro_rules! events {
    ($($(#[$m:meta])* $id:ident = $name:literal, $win:expr;)+) => {
        $( $(#[$m])* pub const $id: Event = Event { name: $name, win_event_id: $win }; )+
        /// 所有事件（測試用：名稱與事件識別碼不得重複）
        #[cfg(test)]
        pub const ALL: &[Event] = &[$($id),+];
    };
}

events! {
    // ---- 1xxx 生命週期 ----
    /// 程式啟動
    AGENT_START = "agent.start", Some(1000);
    /// 正常結束
    AGENT_STOP = "agent.stop", Some(1001);
    /// 設定警告（`code`：`PLS-W0302`、`PLS-W0303`、`PLS-W8101`、`PLS-W8201`、`PLS-W8202`…）
    CONFIG_WARNING = "config.warning", Some(1003);
    /// 已讀取身分
    IDENTITY_LOADED = "identity.loaded", Some(1004);
    /// 新產生 agent_id
    IDENTITY_CREATED = "identity.created", Some(1005);
    /// 啟動失敗或背景任務異常結束
    AGENT_FATAL = "agent.fatal", Some(1020);
    /// 系統資訊取樣逾時（`PLS-W8005`）
    SAMPLE_TIMEOUT = "sample.timeout", Some(1030);
    // ---- 2xxx 連線 ----
    /// 收到 hello_ack，已註冊
    CONN_REGISTERED = "conn.registered", Some(2000);
    /// 連線中斷（一般網路錯誤與其他未細分的原因）
    CONN_LOST = "conn.lost", Some(2001);
    /// 中斷後恢復
    CONN_RESTORED = "conn.restored", Some(2003);
    /// 中斷超過 30 分鐘
    CONN_OUTAGE = "conn.outage", Some(2004);
    /// 伺服器憑證驗證失敗（`PLS-E1401`～`PLS-E1405`）
    TLS_SERVER_CERT_REJECTED = "tls.server_cert_rejected", Some(2100);
    /// `PLS-E1101`
    AUTH_CLIENT_CERT_REQUIRED = "auth.client_cert_required", Some(2200);
    /// `PLS-E1109`
    AUTH_UNTRUSTED = "auth.untrusted", Some(2201);
    /// `PLS-E1105`（`certificate_rejected` 與 TLS alert `CertificateRevoked` 合併）
    AUTH_REJECTED = "auth.rejected", Some(2203);
    /// `PLS-E1108`
    AUTH_EXPIRED = "auth.expired", Some(2204);
    /// `PLS-E1102`、`PLS-E1103`
    AUTH_MISMATCH = "auth.mismatch", Some(2205);
    /// `PLS-E1203`
    AUTH_SERVER_CRL = "auth.server_crl", Some(2208);
    /// `PLS-W3101`（單次）／`PLS-E3102`（連續）
    CONN_SUPERSEDED = "conn.superseded", Some(2209);
    /// `PLS-E2101`
    PROTO_INCOMPATIBLE = "proto.incompatible", Some(2300);
    /// `PLS-E3201`
    SERVER_FULL = "server.full", Some(2301);
    /// `storage_unavailable`（`PLS-W4101`／`PLS-W2203`）、`invalid_report`（`PLS-W2105`）
    REPORT_REJECTED = "report.rejected", Some(2400);
    /// 上一筆尚未回應而略過取樣
    REPORT_SKIPPED = "report.skipped", Some(2401);
    /// 無法解析的行、重複 hello_ack 等（`PLS-W2107`）
    SERVER_UNEXPECTED = "server.unexpected", Some(2402);
    // ---- 3xxx 身分與憑證 ----
    /// 用戶端憑證即將到期（`PLS-W1110`）
    CERT_EXPIRING = "cert.expiring", Some(3000);
    /// 自動或手動更新成功（8443 輪替或 8444 過期續簽）
    CERT_RENEWED = "cert.renewed", Some(3001);
    /// 正常輪替失敗（`PLS-W1111`）
    CERT_RENEW_FAILED = "cert.renew_failed", Some(3002);
    /// 本機憑證已過期（`PLS-E1108`）
    CERT_EXPIRED = "cert.expired", Some(3003);
    /// 達輪替門檻，開始更新（`PLS-I1112`）
    CERT_RENEW_STARTED = "cert.renew_started", Some(3004);
    /// 每次過期續簽嘗試（DEBUG）
    CERT_GRACE_RENEW_ATTEMPT = "cert.grace_renew_attempt", None;
    /// 過期續簽受阻（`PLS-E2007`／`PLS-E2202`／`PLS-E8302`／`PLS-W2107`）
    CERT_GRACE_RENEW_BLOCKED = "cert.grace_renew_blocked", Some(3005);
    /// `PLS-E1113`
    CERT_GRACE_EXCEEDED = "cert.grace_exceeded", Some(3006);
    /// `PLS-E1114`
    CERT_GRACE_REJECTED = "cert.grace_rejected", Some(3007);
    // ---- 4xxx 安裝與註冊 ----
    /// `PLS-W3107`：完全移除時無法通知 Kairos 除役
    INSTALL_DECOMMISSION_UNREPORTED = "install.decommission_unreported", Some(4002);
    /// `PLS-W3310`：無法撤回待核准的註冊申請
    INSTALL_WITHDRAWAL_UNREPORTED = "install.withdrawal_unreported", Some(4005);
    /// 已送出註冊申請
    ENROLL_SUBMITTED = "enroll.submitted", Some(4100);
    /// 等待核准（`PLS-I3307`；只在狀態轉換時）
    ENROLL_PENDING = "enroll.pending", Some(4101);
    /// 已核准（`PLS-I3308`）
    ENROLL_APPROVED = "enroll.approved", Some(4102);
    /// `PLS-E3302`
    ENROLL_REJECTED = "enroll.rejected", Some(4103);
    /// `PLS-E3002`～`PLS-E3004`
    ENROLL_TOKEN_ERROR = "enroll.token_error", Some(4104);
    /// `PLS-E3303`
    ENROLL_REQUEST_EXPIRED = "enroll.request_expired", Some(4105);
    /// 危險操作（`pulse enroll --force`、`--reset-identity`、`--cancel`）
    ADMIN_DANGEROUS_ACTION = "admin.dangerous_action", Some(4107);
    /// `PLS-E2202`：註冊端點回 429／503
    ENROLL_RATE_LIMITED = "enroll.rate_limited", Some(4108);
    /// `PLS-E2008`
    ENROLL_NOT_ACCEPTED = "enroll.not_accepted", Some(4109);
    /// `PLS-E3005`
    ENROLL_SOURCE_BLOCKED = "enroll.source_blocked", Some(4110);
    /// `PLS-E3309`
    ENROLL_QUEUE_FULL = "enroll.queue_full", Some(4111);
    /// `PLS-E8302`：Kairos 因時鐘偏差暫不受理註冊
    ENROLL_CLOCK_REJECTED = "enroll.clock_rejected", Some(4112);
    // ---- 5xxx 系統 ----
    /// `PLS-W8301`／`PLS-E8302`
    CLOCK_SKEW = "clock.skew", Some(5000);
    /// `PLS-W4201`
    LOG_RETENTION_PAUSED = "log.retention_paused", Some(5001);
    /// 本機緩衝已開啟（只寫檔案）
    BUFFER_OPENED = "buffer.opened", Some(5010);
    /// `PLS-E4004`
    BUFFER_UNAVAILABLE = "buffer.unavailable", Some(5011);
    /// `PLS-W4103`
    BUFFER_WRITE_FAILED = "buffer.write_failed", Some(5012);
    /// `PLS-W4202`
    BUFFER_FULL = "buffer.full", Some(5013);
    /// `PLS-W4203`
    BUFFER_EXPIRED = "buffer.expired", Some(5014);
    /// 中斷後的積壓已補送完成
    BUFFER_REPLAY_DONE = "buffer.replay_done", Some(5015);
    /// 無法寫入本機狀態檔（`PLS-E8204`；`log-ux.md` §4.5）：第一次 WARN、連續失敗 60 秒 ERROR
    STATUS_WRITE_FAILED = "status.write_failed", Some(5020);
    /// 恢復寫入本機狀態檔（只在之前記過 `status.write_failed` 時）
    STATUS_WRITE_RESTORED = "status.write_restored", Some(5021);
}

/// 日誌訊息的開頭：「`<event> <code> `」，沒有代碼時為「`<event> - `」
pub fn tag(event: Event, code: Option<&str>) -> String {
    format!("{} {} ", event.name, code.unwrap_or("-"))
}

// ---- 錯誤代碼（copywriting.md §3.3） ----
pub const E_UNKNOWN_ARG: &str = "PLS-E0001";
pub const E_BAD_ARG_VALUE: &str = "PLS-E0002";
pub const E_CONFIG_SYNTAX: &str = "PLS-E0101";
pub const E_CONFIG_RANGE: &str = "PLS-E0201";
pub const W_TLS_VERIFY_DISABLED: &str = "PLS-W0302";
pub const W_BOTH_TRUST_STORES: &str = "PLS-W0303";
pub const E_CONFIG_UNREADABLE: &str = "PLS-E0402";
pub const E_AGENT_ID_INVALID: &str = "PLS-E3103";
pub const E_AGENT_ID_IO: &str = "PLS-E3104";
pub const E_LOG_INIT: &str = "PLS-E4002";
pub const E_CLIENT_CERT_REQUIRED: &str = "PLS-E1101";
pub const E_CLIENT_CERT_MISMATCH: &str = "PLS-E1102";
pub const E_CLIENT_CERT_MULTI_SAN: &str = "PLS-E1103";
pub const E_CLIENT_CERT_REJECTED: &str = "PLS-E1105";
pub const E_CLIENT_CERT_EXPIRED: &str = "PLS-E1108";
pub const E_CLIENT_CERT_UNTRUSTED: &str = "PLS-E1109";
pub const W_CLIENT_CERT_EXPIRING: &str = "PLS-W1110";
pub const E_SERVER_CRL: &str = "PLS-E1203";
pub const E_SERVER_CERT_UNTRUSTED: &str = "PLS-E1401";
pub const E_SERVER_CERT_NAME: &str = "PLS-E1402";
pub const E_SERVER_CERT_VALIDITY: &str = "PLS-E1403";
pub const E_SERVER_CERT_REVOKED: &str = "PLS-E1404";
pub const E_SERVER_CERT_PURPOSE: &str = "PLS-E1405";
pub const E_DNS: &str = "PLS-E2002";
pub const E_CONN_REFUSED: &str = "PLS-E2003";
pub const E_CONN_TIMEOUT: &str = "PLS-E2004";
pub const E_SERVER_SILENT: &str = "PLS-E2005";
pub const E_PROTO_VERSION: &str = "PLS-E2101";
pub const E_HELLO_REQUIRED: &str = "PLS-E2102";
pub const E_LINE_TOO_LONG: &str = "PLS-E2103";
pub const E_HELLO_BAD_REQUEST: &str = "PLS-E2104";
pub const W_INVALID_REPORT: &str = "PLS-W2105";
pub const W_SERVER_UNEXPECTED: &str = "PLS-W2107";
pub const E_PER_IP_LIMIT: &str = "PLS-E2201";
pub const W_REPORT_RATE: &str = "PLS-W2203";
pub const W_SUPERSEDED: &str = "PLS-W3101";
pub const E_AGENT_ID_CONFLICT: &str = "PLS-E3102";
pub const E_REGISTRY_FULL: &str = "PLS-E3201";
pub const E_BUFFER_UNAVAILABLE: &str = "PLS-E4004";
pub const W_SERVER_STORAGE: &str = "PLS-W4101";
pub const W_BUFFER_WRITE: &str = "PLS-W4103";
pub const W_LOG_RETENTION_PAUSED: &str = "PLS-W4201";
pub const W_BUFFER_FULL: &str = "PLS-W4202";
pub const W_BUFFER_EXPIRED: &str = "PLS-W4203";
pub const E_SERVICE_NOT_INSTALLED: &str = "PLS-E8002";
pub const E_SERVICE_NOT_RUNNING: &str = "PLS-E8003";
pub const W_STATUS_STALE: &str = "PLS-W8004";
pub const W_SAMPLE_TIMEOUT: &str = "PLS-W8005";
pub const W_SYSTEM32_WORKDIR: &str = "PLS-W8101";
pub const W_AGENT_ID_PERMS: &str = "PLS-W8201";
pub const W_KEY_PERMS: &str = "PLS-W8202";
pub const E_NEED_ADMIN: &str = "PLS-E8203";
pub const E_STATUS_UNWRITABLE: &str = "PLS-E8204";
pub const W_CLOCK_SKEW: &str = "PLS-W8301";
pub const E_CLOCK_SKEW: &str = "PLS-E8302";
pub const E_CERT_NOT_YET_VALID: &str = "PLS-E8304";
// ---- 註冊與憑證更新（衝刺 4，ENROLL.md） ----
pub const E_ARG_CONFLICT: &str = "PLS-E0003";
pub const E_NON_INTERACTIVE: &str = "PLS-E0004";
pub const E_CLIENT_CERT_LOAD: &str = "PLS-E1104";
pub const E_KEY_MISMATCH: &str = "PLS-E1107";
pub const W_RENEW_FAILED: &str = "PLS-W1111";
pub const I_RENEWING: &str = "PLS-I1112";
pub const E_GRACE_EXCEEDED: &str = "PLS-E1113";
pub const E_GRACE_IDENTITY: &str = "PLS-E1114";
pub const E_CA_PIN: &str = "PLS-E1406";
pub const W_TOFU: &str = "PLS-W1407";
pub const E_ENROLL_UNREACHABLE: &str = "PLS-E2007";
pub const E_ENROLL_NOT_ACCEPTED: &str = "PLS-E2008";
pub const E_ENROLL_UNSUPPORTED: &str = "PLS-E2106";
pub const E_ENROLL_RATE_LIMITED: &str = "PLS-E2202";
pub const E_TOKEN_FORMAT: &str = "PLS-E3001";
pub const E_TOKEN_INVALID: &str = "PLS-E3002";
pub const E_TOKEN_EXPIRED: &str = "PLS-E3003";
pub const E_TOKEN_EXHAUSTED: &str = "PLS-E3004";
pub const E_SOURCE_BLOCKED: &str = "PLS-E3005";
pub const W_DEREGISTER_UNREPORTED: &str = "PLS-W3107";
pub const E_ALREADY_ENROLLED: &str = "PLS-E3301";
pub const E_REQUEST_REJECTED: &str = "PLS-E3302";
pub const E_REQUEST_EXPIRED: &str = "PLS-E3303";
pub const E_REQUEST_INVALID: &str = "PLS-E3304";
pub const E_ENROLL_IN_PROGRESS: &str = "PLS-E3305";
pub const E_NOT_ENROLLED: &str = "PLS-E3306";
pub const I_PENDING_APPROVAL: &str = "PLS-I3307";
pub const I_ISSUED: &str = "PLS-I3308";
pub const E_PENDING_QUEUE_FULL: &str = "PLS-E3309";
pub const W_WITHDRAW_UNREPORTED: &str = "PLS-W3310";
pub const E_LICENSE_EXHAUSTED: &str = "PLS-E7101";
pub const E_CONFIG_NOT_FOUND: &str = "PLS-E0401";
pub const E_DIAG_FAILED: &str = "PLS-E4301";
pub const W_DIAG_TRUNCATED: &str = "PLS-W4302";

/// 程式中有常數的代碼（[`title`] 認得的代碼）；不認得時 `None`
pub fn known(code: &str) -> Option<&'static str> {
    ALL_CODES.iter().copied().find(|c| *c == code)
}

/// 所有代碼常數
const ALL_CODES: &[&str] = &[
    E_UNKNOWN_ARG,
    E_BAD_ARG_VALUE,
    E_CONFIG_SYNTAX,
    E_CONFIG_RANGE,
    W_TLS_VERIFY_DISABLED,
    W_BOTH_TRUST_STORES,
    E_CONFIG_UNREADABLE,
    E_AGENT_ID_INVALID,
    E_AGENT_ID_IO,
    E_LOG_INIT,
    E_CLIENT_CERT_REQUIRED,
    E_CLIENT_CERT_MISMATCH,
    E_CLIENT_CERT_MULTI_SAN,
    E_CLIENT_CERT_REJECTED,
    E_CLIENT_CERT_EXPIRED,
    E_CLIENT_CERT_UNTRUSTED,
    W_CLIENT_CERT_EXPIRING,
    E_SERVER_CRL,
    E_SERVER_CERT_UNTRUSTED,
    E_SERVER_CERT_NAME,
    E_SERVER_CERT_VALIDITY,
    E_SERVER_CERT_REVOKED,
    E_SERVER_CERT_PURPOSE,
    E_DNS,
    E_CONN_REFUSED,
    E_CONN_TIMEOUT,
    E_SERVER_SILENT,
    E_PROTO_VERSION,
    E_HELLO_REQUIRED,
    E_LINE_TOO_LONG,
    E_HELLO_BAD_REQUEST,
    W_INVALID_REPORT,
    W_SERVER_UNEXPECTED,
    E_PER_IP_LIMIT,
    W_REPORT_RATE,
    W_SUPERSEDED,
    E_AGENT_ID_CONFLICT,
    E_REGISTRY_FULL,
    E_BUFFER_UNAVAILABLE,
    W_SERVER_STORAGE,
    W_BUFFER_WRITE,
    W_LOG_RETENTION_PAUSED,
    W_BUFFER_FULL,
    W_BUFFER_EXPIRED,
    E_SERVICE_NOT_INSTALLED,
    E_SERVICE_NOT_RUNNING,
    W_STATUS_STALE,
    W_SAMPLE_TIMEOUT,
    W_SYSTEM32_WORKDIR,
    W_AGENT_ID_PERMS,
    W_KEY_PERMS,
    E_NEED_ADMIN,
    E_STATUS_UNWRITABLE,
    W_CLOCK_SKEW,
    E_CLOCK_SKEW,
    E_CERT_NOT_YET_VALID,
    E_ARG_CONFLICT,
    E_NON_INTERACTIVE,
    E_CLIENT_CERT_LOAD,
    E_KEY_MISMATCH,
    W_RENEW_FAILED,
    I_RENEWING,
    E_GRACE_EXCEEDED,
    E_GRACE_IDENTITY,
    E_CA_PIN,
    W_TOFU,
    E_ENROLL_UNREACHABLE,
    E_ENROLL_NOT_ACCEPTED,
    E_ENROLL_UNSUPPORTED,
    E_ENROLL_RATE_LIMITED,
    E_TOKEN_FORMAT,
    E_TOKEN_INVALID,
    E_TOKEN_EXPIRED,
    E_TOKEN_EXHAUSTED,
    E_SOURCE_BLOCKED,
    W_DEREGISTER_UNREPORTED,
    E_ALREADY_ENROLLED,
    E_REQUEST_REJECTED,
    E_REQUEST_EXPIRED,
    E_REQUEST_INVALID,
    E_ENROLL_IN_PROGRESS,
    E_NOT_ENROLLED,
    I_PENDING_APPROVAL,
    I_ISSUED,
    E_PENDING_QUEUE_FULL,
    W_WITHDRAW_UNREPORTED,
    E_LICENSE_EXHAUSTED,
    E_CONFIG_NOT_FOUND,
    E_DIAG_FAILED,
    W_DIAG_TRUNCATED,
];

/// 代碼的標題（`copywriting.md` §4.2 的範例標題；沒有範例的依 §3.3「情況」欄改寫）。
/// 不認得的代碼回傳 `None`。
pub fn title(code: &str) -> Option<&'static str> {
    Some(match code {
        E_UNKNOWN_ARG => "未知的參數或命令",
        E_BAD_ARG_VALUE => "參數值不合法",
        E_CONFIG_SYNTAX => "設定檔語法錯誤",
        E_CONFIG_RANGE => "設定值超出範圍",
        W_TLS_VERIFY_DISABLED => "已停用伺服器憑證驗證",
        W_BOTH_TRUST_STORES => "同時信任系統信任庫與私有 CA",
        E_CONFIG_UNREADABLE => "設定檔無法讀取",
        E_CONFIG_NOT_FOUND => "指定的設定檔不存在",
        E_DIAG_FAILED => "無法寫出診斷包",
        W_DIAG_TRUNCATED => "診斷包內容已截斷",
        E_AGENT_ID_INVALID => "agent.id 內容不合法",
        E_AGENT_ID_IO => "agent.id 無法建立或讀取",
        E_LOG_INIT => "無法初始化日誌",
        E_CLIENT_CERT_REQUIRED => "無法註冊：伺服器要求用戶端憑證，但本機沒有設定",
        E_CLIENT_CERT_MISMATCH => "無法註冊：用戶端憑證與 agent_id 不符",
        E_CLIENT_CERT_MULTI_SAN => "無法註冊：用戶端憑證含多個 DNS SAN",
        E_CLIENT_CERT_REJECTED => "無法連線：這台主機的憑證已被撤銷",
        E_CLIENT_CERT_EXPIRED => "用戶端憑證已過期",
        E_CLIENT_CERT_UNTRUSTED => "無法註冊：Kairos 不信任這台主機的憑證簽發者",
        W_CLIENT_CERT_EXPIRING => "用戶端憑證即將到期，且不會自動更新",
        E_SERVER_CRL => "伺服器拒絕連線：伺服器的撤銷清單已過期",
        E_SERVER_CERT_UNTRUSTED => "無法連線：伺服器憑證不受信任",
        E_SERVER_CERT_NAME => "無法連線：伺服器憑證的名稱不符",
        E_SERVER_CERT_VALIDITY => "無法連線：伺服器憑證已過期或尚未生效",
        E_SERVER_CERT_REVOKED => "無法連線：伺服器憑證已被撤銷",
        E_SERVER_CERT_PURPOSE => "無法連線：伺服器憑證用途不符",
        E_DNS => "無法連線到 Kairos（找不到主機名稱）",
        E_CONN_REFUSED => "無法連線到 Kairos（連線被拒絕）",
        E_CONN_TIMEOUT => "無法連線到 Kairos（連線逾時）",
        E_SERVER_SILENT => "Kairos 沒有回應",
        E_PROTO_VERSION => "無法註冊：協定版本不相容",
        E_HELLO_REQUIRED => "無法註冊：Kairos 回應第一則訊息不是 hello",
        E_LINE_TOO_LONG => "無法註冊：訊息超過單行上限",
        E_HELLO_BAD_REQUEST => "無法註冊：Kairos 認為註冊資料不合法",
        W_INVALID_REPORT => "Kairos 拒收回報資料",
        W_SERVER_UNEXPECTED => "Kairos 的回應無法解析或不合預期",
        E_PER_IP_LIMIT => "無法連線：同一 IP 的連線數超過 Kairos 的上限",
        W_REPORT_RATE => "回報速率超過 Kairos 的上限",
        W_SUPERSEDED => "連線被同一 agent_id 的新連線取代",
        E_AGENT_ID_CONFLICT => "agent_id 衝突：多台主機使用同一個 agent_id",
        E_REGISTRY_FULL => "無法註冊：Kairos 的主機狀態表已滿",
        E_BUFFER_UNAVAILABLE => "無法開啟本機緩衝，斷線期間的資料不會補送",
        W_SERVER_STORAGE => "伺服器暫時無法保存回報資料",
        W_BUFFER_WRITE => "無法寫入本機緩衝，部分取樣遺失",
        W_LOG_RETENTION_PAUSED => "系統日期異常，暫停清除過期日誌",
        W_BUFFER_FULL => "本機緩衝已滿，最舊的待補送資料已丟棄",
        W_BUFFER_EXPIRED => "待補送資料超過保存時間，已丟棄",
        E_SERVICE_NOT_INSTALLED => "Pulse 服務未安裝",
        E_SERVICE_NOT_RUNNING => "Pulse 服務未執行",
        W_STATUS_STALE => "狀態檔過時，Pulse 可能卡住",
        W_SAMPLE_TIMEOUT => "系統資訊取樣逾時",
        W_SYSTEM32_WORKDIR => "Windows 服務的工作目錄是 System32",
        W_AGENT_ID_PERMS => "agent.id 權限過寬",
        W_KEY_PERMS => "私鑰檔案權限過寬",
        E_NEED_ADMIN => "需要系統管理員權限",
        E_STATUS_UNWRITABLE => "無法寫入狀態檔",
        W_CLOCK_SKEW => "本機時鐘與 Kairos 相差超過 30 秒",
        E_CLOCK_SKEW => "本機時鐘與 Kairos 相差超過 5 分鐘",
        E_CERT_NOT_YET_VALID => "用戶端憑證尚未生效（本機時鐘可能落後）",
        E_ARG_CONFLICT => "參數互斥",
        E_NON_INTERACTIVE => "非互動模式缺少必要參數",
        E_CLIENT_CERT_LOAD => "用戶端憑證或私鑰無法載入",
        E_KEY_MISMATCH => "私鑰與憑證不成對",
        W_RENEW_FAILED => "用戶端憑證自動更新失敗",
        I_RENEWING => "正在更新用戶端憑證",
        E_GRACE_EXCEEDED => "憑證過期已超過過期寬限期，無法自動續簽",
        E_GRACE_IDENTITY => "Kairos 拒絕自動續簽：主機識別資訊與紀錄不符",
        E_CA_PIN => "無法驗證 Kairos 身分（CA 指紋不符或未提供）",
        W_TOFU => "未驗證伺服器身分",
        E_ENROLL_UNREACHABLE => "無法連線到註冊端點",
        E_ENROLL_NOT_ACCEPTED => "無法註冊：Kairos 目前不受理新的註冊申請",
        E_ENROLL_UNSUPPORTED => "無法註冊：Kairos 版本不支援自動註冊",
        E_ENROLL_RATE_LIMITED => "註冊申請暫緩送出：註冊端點忙碌中",
        E_TOKEN_FORMAT => "註冊 token 格式錯誤",
        E_TOKEN_INVALID => "註冊 token 無效",
        E_TOKEN_EXPIRED => "註冊 token 已過期",
        E_TOKEN_EXHAUSTED => "註冊 token 使用次數已滿",
        E_SOURCE_BLOCKED => "此網路位址暫時被封鎖",
        W_DEREGISTER_UNREPORTED => "無法通知 Kairos 這台主機已除役",
        E_ALREADY_ENROLLED => "這台主機已註冊",
        E_REQUEST_REJECTED => "註冊申請被拒絕",
        E_REQUEST_EXPIRED => "註冊申請已逾期",
        E_REQUEST_INVALID => "註冊申請內容不合法",
        E_ENROLL_IN_PROGRESS => "已有進行中的註冊程序",
        E_NOT_ENROLLED => "尚未註冊",
        I_PENDING_APPROVAL => "等待核准",
        I_ISSUED => "已核准，已取得憑證",
        E_PENDING_QUEUE_FULL => "Kairos 的待核准佇列已滿",
        W_WITHDRAW_UNREPORTED => "無法向 Kairos 撤回註冊申請",
        E_LICENSE_EXHAUSTED => "授權節點數已滿",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_put_event_then_code_first() {
        assert_eq!(
            tag(BUFFER_FULL, Some(W_BUFFER_FULL)),
            "buffer.full PLS-W4202 "
        );
        assert_eq!(tag(BUFFER_OPENED, None), "buffer.opened - ");
    }

    #[test]
    fn event_names_and_ids_follow_log_ux() {
        let mut names = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        for e in ALL {
            assert!(names.insert(e.name), "重複的事件名稱 {}", e.name);
            if let Some(id) = e.win_event_id {
                assert!(ids.insert(id), "重複的事件識別碼 {id}");
            }
            let (domain, rest) = e.name.split_once('.').expect(e.name);
            assert!(
                !domain.is_empty()
                    && !rest.is_empty()
                    && e.name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_'),
                "{}",
                e.name
            );
        }
        // INTEGRATION §25：本機緩衝的事件 5010～5015
        let buffer: Vec<(&str, u16)> = ALL
            .iter()
            .filter(|e| e.name.starts_with("buffer."))
            .map(|e| (e.name, e.win_event_id.unwrap()))
            .collect();
        assert_eq!(
            buffer,
            [
                ("buffer.opened", 5010),
                ("buffer.unavailable", 5011),
                ("buffer.write_failed", 5012),
                ("buffer.full", 5013),
                ("buffer.expired", 5014),
                ("buffer.replay_done", 5015),
            ]
        );
        // INTEGRATION §26：本機狀態檔的事件 5020、5021
        assert_eq!(STATUS_WRITE_FAILED.win_event_id, Some(5020));
        assert_eq!(STATUS_WRITE_RESTORED.win_event_id, Some(5021));
        assert_eq!(CLOCK_SKEW.win_event_id, Some(5000));
    }

    #[test]
    fn every_code_constant_has_a_title_and_a_valid_format() {
        for &code in ALL_CODES {
            assert!(title(code).is_some(), "{code}");
            assert_eq!(known(code), Some(code));
        }
        assert_eq!(known("PLS-E9999"), None);
        for code in [
            E_CONFIG_SYNTAX,
            E_CLIENT_CERT_REJECTED,
            E_CONN_REFUSED,
            E_BUFFER_UNAVAILABLE,
            W_BUFFER_WRITE,
            W_BUFFER_FULL,
            W_BUFFER_EXPIRED,
            W_STATUS_STALE,
            E_SERVICE_NOT_RUNNING,
            E_STATUS_UNWRITABLE,
            E_CLOCK_SKEW,
        ] {
            assert!(title(code).is_some(), "{code}");
            let b = code.as_bytes();
            assert!(
                code.len() == 9
                    && code.starts_with("PLS-")
                    && matches!(b[4], b'E' | b'W' | b'I')
                    && b[5..].iter().all(u8::is_ascii_digit),
                "{code}"
            );
        }
        assert_eq!(title("PLS-E9999"), None);
    }
}
