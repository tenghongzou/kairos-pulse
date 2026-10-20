//! `pki_cas`、`pki_certificates`、`pki_crls` 的讀寫（schema v4，`kairos/PKI.md` 第 7 節）。
//!
//! 所有函式都是同步的，呼叫端在 blocking 執行緒（或 CLI）上使用；寫入多筆的操作由呼叫端包在交易中。
// 查詢結果以 tuple 回傳（沿用原本查詢的形狀），方法參數與原本的函式相同
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

use super::issue::{Reason, RevokedEntry};
use crate::store::db::StoreResult;

/// `pki_cas.state`
pub const CA_PENDING: &str = "pending";

pub const CA_ACTIVE: &str = "active";

pub const CA_TRUSTED: &str = "trusted";

pub const CA_RETIRED: &str = "retired";

/// CRL 收錄撤銷的期限：憑證過期超過 1 天後不再收錄（`kairos/PKI.md` 第 10 節）
pub const CRL_KEEP_EXPIRED_MS: i64 = 86_400_000;

/// 每個 CA 保留的 CRL 版本數
pub const CRL_HISTORY: i64 = 100;

/// 代理簽發 CA
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaRow {
    pub ca_id: i64,
    pub subject: String,
    pub spki_sha256: Vec<u8>,
    pub cert_der: Option<Vec<u8>>,
    pub not_before: Option<i64>,
    pub not_after: Option<i64>,
    pub key_file: Option<String>,
    pub state: String,
    pub next_crl_number: i64,
    pub created_at: i64,
    pub retired_at: Option<i64>,
}

/// 簽發紀錄
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CertRow {
    pub serial: String,
    pub ca_id: Option<i64>,
    pub agent_id: String,
    pub spki_sha256: Vec<u8>,
    pub not_before: i64,
    pub not_after: i64,
    pub cert_der: Vec<u8>,
    pub issued_at: i64,
    pub issued_via: String,
    pub request_id: Option<String>,
    pub approved_by: Option<String>,
    pub first_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub revoke_effective_at: Option<i64>,
    pub revocation_reason: Option<String>,
    pub revoked_by: Option<String>,
    pub revoke_comment: Option<String>,
    pub crl_number: Option<i64>,
}

impl CertRow {
    /// 撤銷已生效（`revoke_effective_at` 已到）
    pub fn revoked_effective(&self, now_ms: i64) -> bool {
        self.revoke_effective_at.is_some_and(|t| t <= now_ms)
    }
}

/// 撤銷的結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revoke {
    Now(CertRow),
    Already(CertRow),
    NotFound,
}

/// CRL 的一個版本
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrlRow {
    pub ca_id: i64,
    pub crl_number: i64,
    pub this_update: i64,
    pub next_update: i64,
    pub revoked_count: i64,
    pub der: Vec<u8>,
    pub source: String,
    pub created_by: Option<String>,
}

/// 簽發 CA、已簽發的憑證、撤銷與 CRL 的儲存（`pki_cas`、`pki_certs`、`pki_crls`）。SQLite 的實作在
/// `store/sqlite/pki.rs`
///
/// CRL 序號（`kairos/HA.md` 第 5.4 節 D5）：以 [`PkiStore::get_ca`] 讀出 `next_crl_number`、簽章後以
/// [`PkiStore::record_crl`] 推進，兩者必須在同一個 [`TxMode::Immediate`](crate::store::db::TxMode::Immediate)
/// 的交易中（SQLite 以全域寫入鎖序列化；PostgreSQL 後端改以 `SELECT … FOR UPDATE` 加上 leader 的 fencing）
pub trait PkiStore {
    /// 已撤銷的所有序號（告警判定用）
    fn revoked_serials(&self) -> StoreResult<std::collections::HashSet<String>>;

    /// 新增一筆 `pending` 的簽發 CA，回傳 `ca_id`；私鑰檔名由 `key_file(ca_id)` 決定
    fn insert_pending_ca(
        &self,
        subject_of: &dyn Fn(i64) -> String,
        spki_sha256: &[u8],
        key_file: &dyn Fn(i64) -> String,
        now_ms: i64,
    ) -> StoreResult<i64>;

    /// 下一個 `ca_id`（產生 CSR 時 subject 要含 ca_id）
    fn next_ca_id(&self) -> StoreResult<i64>;

    fn list_cas(&self) -> StoreResult<Vec<CaRow>>;

    fn get_ca(&self, ca_id: i64) -> StoreResult<Option<CaRow>>;

    fn find_ca_by_spki(&self, spki_sha256: &[u8]) -> StoreResult<Option<CaRow>>;

    /// 安裝根 CA 簽發的憑證：狀態改為 `trusted`；沒有 `active` 的 CA 時同時啟用（第一個簽發 CA）。
    /// 回傳是否已啟用
    fn install_ca(
        &self,
        ca_id: i64,
        subject: &str,
        cert_der: &[u8],
        not_before: i64,
        not_after: i64,
    ) -> StoreResult<bool>;

    /// 啟用：新簽發改用這個 CA（其他 `active` 改為 `trusted`）
    fn activate_ca(&self, ca_id: i64) -> StoreResult<()>;

    /// 退役：從信任錨移除
    fn retire_ca(&self, ca_id: i64, now_ms: i64) -> StoreResult<()>;

    /// 目前的信任錨（`active`、`trusted` 且已安裝憑證）
    fn trust_anchor_cas(&self) -> StoreResult<Vec<CaRow>>;

    fn insert_cert(&self, c: &CertRow) -> StoreResult<()>;

    fn serial_exists(&self, serial: &str) -> StoreResult<bool>;

    fn get_cert(&self, serial: &str) -> StoreResult<Option<CertRow>>;

    /// 某台主機的所有憑證（新到舊）
    fn certs_of_agent(&self, agent_id: &str) -> StoreResult<Vec<CertRow>>;

    /// 列表（主控台、CLI）：新到舊，最多 `limit` 筆，`before` 為上一頁最後一筆的 (issued_at, serial)
    fn list_certs(&self, limit: usize, before: Option<(i64, &str)>) -> StoreResult<Vec<CertRow>>;

    /// 這台主機先前的憑證是否用過這把公鑰（換鑰才算輪替，ENROLL.md 第 15.1 節）
    fn spki_used_by_agent(&self, agent_id: &str, spki_sha256: &[u8]) -> StoreResult<bool>;

    /// 撤銷（立即寫入；`effective_at` 為進入 CRL 的時間，一般為現在、除役為 +10 分鐘）
    fn revoke(
        &self,
        serial: &str,
        at_ms: i64,
        effective_at_ms: i64,
        reason: Reason,
        by: &str,
        comment: Option<&str>,
    ) -> StoreResult<Revoke>;

    /// CRL 收錄的撤銷：這個 CA、已生效、憑證過期未超過 1 天
    fn revocations_for_crl(&self, ca_id: i64, now_ms: i64) -> StoreResult<Vec<RevokedEntry>>;

    /// 已生效、但還沒有收錄到任何 CRL 的撤銷（CLI 寫入或延後生效的撤銷到期時，常駐行程據此重新產生 CRL）
    fn ca_ids_with_pending_revocations(&self, now_ms: i64) -> StoreResult<Vec<i64>>;

    /// 記錄新的 CRL：寫入 `pki_crls`、遞增 `next_crl_number`、標記這次收錄的撤銷，並只保留最近 100 版
    #[allow(clippy::too_many_arguments)]
    fn record_crl(
        &self,
        ca_id: i64,
        crl_number: i64,
        this_update: i64,
        next_update: i64,
        included: &[RevokedEntry],
        der: &[u8],
        source: &str,
        created_by: Option<&str>,
    ) -> StoreResult<()>;

    fn latest_crl(&self, ca_id: i64) -> StoreResult<Option<CrlRow>>;

    /// CRL 版本歷史（新到舊，不含 DER 以外的大欄位時可再精簡）
    fn crl_history(&self, ca_id: i64, limit: usize) -> StoreResult<Vec<CrlRow>>;

    /// 第一次以這張憑證成功 hello：寫入 `first_used_at`。回傳是否是第一次（呼叫端據此撤銷舊憑證）
    fn mark_first_used(&self, serial: &str, now_ms: i64) -> StoreResult<bool>;

    /// 同一台主機在 `serial` 之前簽發、尚未撤銷的憑證（輪替成功後以 `superseded` 撤銷）
    fn older_unrevoked(&self, agent_id: &str, serial: &str) -> StoreResult<Vec<String>>;

    /// 依簽發 CA 統計簽發張數
    fn issued_count_by_ca(&self, ca_id: i64) -> StoreResult<i64>;

    /// 外部簽發（`ca_id IS NULL`）、撤銷已生效、憑證過期不早於 `expired_after_ms` 的序號
    fn revoked_external_serials(
        &self,
        now_ms: i64,
        expired_after_ms: i64,
    ) -> StoreResult<std::collections::HashSet<String>>;

    /// 刪除主機資料時為了 CRL 保留的已撤銷憑證：過期超過 1 天、主機已不在清冊的刪除（`kairos/PRIVACY.md` 第 4 節）
    fn delete_orphan_certificates(&self, now_ms: i64) -> StoreResult<usize>;

    /// 刪除主機資料前撤銷這台主機尚未撤銷、尚未過期的憑證（每張一筆稽核，一個交易）；回傳撤銷的序號
    fn revoke_active_certificates(
        &self,
        agent_id: &str,
        actor: &str,
        actor_ip: Option<&str>,
        now_ms: i64,
    ) -> StoreResult<Vec<String>>;

    /// 更改簽發 CA 的私鑰檔名（並行建立時編號不同，私鑰檔依資料庫的編號重新命名）
    fn set_ca_key_file(&self, ca_id: i64, key_file: &str) -> StoreResult<()>;

    /// 清冊中這台主機的最新憑證：(序號, 到期)；清冊沒有這台主機時為 `None`
    fn host_current_cert(
        &self,
        agent_id: &str,
    ) -> StoreResult<Option<(Option<String>, Option<i64>)>>;

    /// 匯入既有憑證時清冊沒有這台主機：新增（`source = 'cert_import'`，主機名稱暫用 agent_id）
    fn insert_imported_host(&self, agent_id: &str, now_ms: i64, serial: &str) -> StoreResult<()>;

    /// 把清冊中這台主機的最新憑證改為 `serial`
    fn set_host_current_serial(&self, agent_id: &str, serial: &str) -> StoreResult<()>;
}
