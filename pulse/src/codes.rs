//! 日誌事件與錯誤代碼（設計包 `pulse/log-ux.md` §4、`design-system/copywriting.md` §3.3）。
//!
//! 有對應事件的日誌行，訊息開頭為「`<event> <code> `」（沒有代碼時為 `-`），順序與 `log-ux.md` §6.1
//! 的欄位相同：
//!
//! ```text
//! 2026-10-13 14:22:16 [WARN] buffer.full PLS-W4202 本機緩衝已滿（待送 50000 筆…），丟掉最舊的資料…
//! ```
//!
//! 規則與搜尋只需要比對 `event` 與 `code`（固定、不翻譯），不依賴訊息文字。行首的時間與等級格式、
//! `log.format = "json"` 等其餘 `log-ux.md` 的改版尚未實作（見 `ERROR-CODES.md`「待辦」）。
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
    /// 本機憑證已過期（`PLS-E1108`）
    CERT_EXPIRED = "cert.expired", Some(3003);
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
    /// 無法寫入本機狀態檔（`PLS-E8204`）。`log-ux.md` §4 沒有這個事件，工程先選用（待設計補登），
    /// 只寫檔案
    STATUS_WRITE_FAILED = "status.write_failed", None;
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

/// 代碼的標題（`copywriting.md` §4.2 的範例標題；沒有範例的依 §3.3「情況」欄改寫）。
/// 不認得的代碼回傳 `None`。
pub fn title(code: &str) -> Option<&'static str> {
    Some(match code {
        E_CONFIG_SYNTAX => "設定檔語法錯誤",
        E_CONFIG_RANGE => "設定值超出範圍",
        W_TLS_VERIFY_DISABLED => "已停用伺服器憑證驗證",
        W_BOTH_TRUST_STORES => "同時信任系統信任庫與私有 CA",
        E_CONFIG_UNREADABLE => "設定檔無法讀取",
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
    }

    #[test]
    fn every_code_constant_has_a_title_and_a_valid_format() {
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
