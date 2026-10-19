//! 常駐行程的簽發器（`kairos/PKI.md` 第 3、10 節）：持有線上簽發 CA 的私鑰、依申請簽發、產生 CRL，
//! 並把信任錨（`pki/issuing-cas.pem`）與 CRL（`pki/crl/issuing.crl`）寫成檔案供 8443 的 TLS 驗證器載入。
//!
//! - 只有常駐行程讀取私鑰；CLI 不簽發（只把申請標為已核准，常駐行程在 2 秒內簽發）。
//! - 私鑰檔或目錄的權限過寬、擁有者不是執行 Kairos 的帳號時**不使用**這把金鑰：線上簽發停用，
//!   核准的申請停在「已核准，待簽發」，記 `ERROR` 與 `KRS-E1303`（不拒絕啟動：既有主機照常連線）。
//!   離線簽發模式（選擇的結果，不是錯誤）的代碼為 `KRS-E1302`；簽發 CA 即將到期為 `KRS-W1304`。
//! - 一筆簽發＝產生序號 → 依範本簽章 → 單一交易寫入 `pki_certificates`、`enroll_requests`（`issued`）、
//!   `hosts`（`current_serial`）、`audit_events`。交易失敗時憑證視同未簽發。

use log::{error, info};
use rusqlite::{Connection, TransactionBehavior};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use super::db::DbError;
use super::store::{self as es, Audit, DbKind, DbState, RequestRow};
use crate::config::PkiMode;
use crate::pki::issue::{IssuingCa, random_serial};
use crate::pki::store::{self as ps, CA_ACTIVE, CA_RETIRED, CA_TRUSTED, CertRow};
use crate::pki::x509::{parse_cert, pem_decode_all, pem_encode};
use crate::pki::{self, csr::check_csr};
use crate::settings::Effective;
use crate::store::prelude::*;

/// 線上簽發的狀態
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueStatus {
    /// 可以簽發（啟用中的簽發 CA）
    Online { ca_id: i64 },
    /// 離線簽發模式（`pki.mode = "offline"` 或未設定）
    Offline,
    /// 線上模式但無法簽發（沒有啟用的簽發 CA、私鑰權限過寬或無法讀取等）
    Unavailable(String),
}

/// Pulse 信任的 CA（`pki.server_ca_path`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerCa {
    pub pem: String,
    pub der: Vec<u8>,
    pub spki: Vec<u8>,
    /// `sha256:…`（安裝字串的 CA 指紋）
    pub pin: String,
}

/// 讀取 Pulse 信任的 CA
pub fn load_server_ca(path: &str) -> Result<ServerCa, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("無法讀取 {path}：{e}"))?;
    let blocks = pem_decode_all(&text, Some("CERTIFICATE")).map_err(|e| format!("{path}：{e}"))?;
    let Some((_, der)) = blocks.into_iter().next() else {
        return Err(format!("{path} 中找不到 CA 憑證"));
    };
    let info = parse_cert(&der).map_err(|e| format!("{path}：{e}"))?;
    if !info.is_ca {
        return Err(format!(
            "{path} 不是 CA 憑證（basicConstraints 必須是 CA:TRUE）；pki.server_ca_path 應指向 Pulse 信任的根 CA"
        ));
    }
    Ok(ServerCa {
        pem: pem_encode("CERTIFICATE", &der),
        pin: kairos_proto::enroll::ca_pin_from_spki(&info.spki_der),
        spki: info.spki_der,
        der,
    })
}

impl IssueStatus {
    /// `issuance_unavailable` 的產品代碼：離線簽發 `KRS-E1302`、線上但私鑰無法使用 `KRS-E1303`
    pub fn product_code(&self) -> Option<&'static str> {
        match self {
            IssueStatus::Online { .. } => None,
            IssueStatus::Offline => Some("KRS-E1302"),
            IssueStatus::Unavailable(_) => Some("KRS-E1303"),
        }
    }
}

/// 簽發的結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueOutcome {
    Issued(CertRow),
    /// 已簽發過（冪等：回傳同一張）
    Already(CertRow),
    /// 申請的狀態不允許簽發（已拒絕、已撤回等）
    NotIssuable(DbState),
    /// 申請不存在
    NotFound,
}

/// 簽發失敗
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueError {
    /// 線上簽發不可用（離線模式、私鑰無法使用）
    Unavailable(String),
    /// CSR 或簽章失敗（寫入 `issue_error`）
    Failed(String),
    Db(DbError),
}

impl From<DbError> for IssueError {
    fn from(e: DbError) -> Self {
        IssueError::Db(e)
    }
}

impl From<rusqlite::Error> for IssueError {
    fn from(e: rusqlite::Error) -> Self {
        IssueError::Db(e.into())
    }
}

impl From<crate::store::db::StoreError> for IssueError {
    fn from(e: crate::store::db::StoreError) -> Self {
        IssueError::Db(e.into())
    }
}

/// 簽發器與信任錨、CRL 的檔案
pub struct PkiManager {
    data_dir: PathBuf,
    mode: PkiMode,
    /// 持有私鑰的簽發 CA（啟用中與信任中；產生 CRL）
    issuers: RwLock<HashMap<i64, Arc<IssuingCa>>>,
    status: RwLock<IssueStatus>,
    /// 上次寫出的信任錨（ca_id, 狀態）；有變化才重寫檔案
    anchors: Mutex<Option<Vec<(i64, String)>>>,
    /// 上次寫出的已退役簽發 CA（ca_id）；有變化才重寫 `pki/issuing-cas-retired.pem`
    retired_written: Mutex<Option<Vec<i64>>>,
    /// 已退役簽發 CA 的憑證（DER）：8444 的過期續簽拒絕它們簽出的憑證
    retired: RwLock<Vec<Vec<u8>>>,
    /// 已記錄過的私鑰問題（避免每 2 秒重複記錄）
    logged_problems: Mutex<HashMap<i64, String>>,
    pub server_ca: RwLock<Option<ServerCa>>,
}

impl PkiManager {
    pub fn new(data_dir: &Path, mode: PkiMode) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            mode,
            issuers: RwLock::new(HashMap::new()),
            status: RwLock::new(match mode {
                PkiMode::Offline => IssueStatus::Offline,
                PkiMode::Online => IssueStatus::Unavailable("尚未載入簽發 CA".into()),
            }),
            anchors: Mutex::new(None),
            retired_written: Mutex::new(None),
            retired: RwLock::new(Vec::new()),
            logged_problems: Mutex::new(HashMap::new()),
            server_ca: RwLock::new(None),
        }
    }

    pub fn mode(&self) -> PkiMode {
        self.mode
    }

    pub fn status(&self) -> IssueStatus {
        self.status
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn server_ca(&self) -> Option<ServerCa> {
        self.server_ca
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 已退役簽發 CA 的憑證（DER；上次與資料庫同步時）
    pub fn retired_ca_ders(&self) -> Vec<Vec<u8>> {
        self.retired
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn issuer(&self, ca_id: i64) -> Option<Arc<IssuingCa>> {
        self.issuers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ca_id)
            .cloned()
    }

    /// 可以簽發時回傳啟用中的簽發 CA
    pub fn active_issuer(&self) -> Result<Arc<IssuingCa>, IssueError> {
        match self.status() {
            IssueStatus::Online { ca_id } => self
                .issuer(ca_id)
                .ok_or_else(|| IssueError::Unavailable("簽發 CA 的私鑰未載入".into())),
            IssueStatus::Offline => Err(IssueError::Unavailable(
                "離線簽發模式（pki.mode = \"offline\"）：核准後需在 CA 主機上簽發".into(),
            )),
            IssueStatus::Unavailable(r) => Err(IssueError::Unavailable(r)),
        }
    }

    /// 由資料庫同步簽發 CA：載入私鑰（線上模式）、更新簽發狀態；信任錨或已退役的簽發 CA 有變化時重寫
    /// `pki/issuing-cas.pem`／`pki/issuing-cas-retired.pem` 並回傳 `true`（呼叫端重建 8443 的 TLS 設定）
    pub fn sync_cas(&self, conn: &Connection) -> Result<bool, DbError> {
        let cas = conn.list_cas()?;
        let retired_changed = self.sync_retired(&cas);
        Ok(self.sync_active(&cas)? | retired_changed)
    }

    /// 已退役的簽發 CA（PKI.md 第 15.4 節）：`auth.client_ca` 為根 CA 或代理 CA 時，用戶端自附退役簽發 CA 的
    /// 憑證就能經由那一組驗證器通過，因此 8443 的驗證器另讀這個檔案、拒絕經過它們的鏈。
    /// 檔案有變化時回傳 `true`
    fn sync_retired(&self, cas: &[ps::CaRow]) -> bool {
        let retired: Vec<&ps::CaRow> = cas
            .iter()
            .filter(|c| c.state == CA_RETIRED && c.cert_der.is_some())
            .collect();
        *self.retired.write().unwrap_or_else(|e| e.into_inner()) =
            retired.iter().filter_map(|c| c.cert_der.clone()).collect();
        let ids: Vec<i64> = retired.iter().map(|c| c.ca_id).collect();
        let path = pki::retired_cas_path(&self.data_dir);
        let mut last = self
            .retired_written
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if last.as_ref() == Some(&ids) && (ids.is_empty() || path.exists()) {
            return false;
        }
        if ids.is_empty() && !path.exists() {
            *last = Some(ids);
            return false;
        }
        let pem: String = retired
            .iter()
            .filter_map(|c| c.cert_der.as_deref())
            .map(|der| pem_encode("CERTIFICATE", der))
            .collect();
        if let Err(e) = pki::ensure_pki_dir(&self.data_dir)
            .and_then(|_| pki::write_public_atomically(&path, pem.as_bytes()))
        {
            error!("無法寫入已退役簽發 CA 的清單 {}：{e}", path.display());
            return false;
        }
        *last = Some(ids);
        true
    }

    fn sync_active(&self, cas: &[ps::CaRow]) -> Result<bool, DbError> {
        let mut issuers = HashMap::new();
        let mut active = None;
        let mut problem = None;
        for ca in cas {
            if ca.state != CA_ACTIVE && ca.state != CA_TRUSTED {
                continue;
            }
            if ca.state == CA_ACTIVE {
                active = Some(ca.ca_id);
            }
            if self.mode != PkiMode::Online {
                continue;
            }
            let (Some(cert), Some(key_file)) = (&ca.cert_der, &ca.key_file) else {
                continue;
            };
            if let Some(existing) = self.issuer(ca.ca_id)
                && existing.cert_der == *cert
            {
                issuers.insert(ca.ca_id, existing);
                continue;
            }
            match self.load_key(ca.ca_id, cert, key_file) {
                Ok(i) => {
                    self.logged_problems
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&ca.ca_id);
                    issuers.insert(ca.ca_id, Arc::new(i));
                }
                Err(reason) => {
                    let mut logged = self
                        .logged_problems
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if logged.get(&ca.ca_id) != Some(&reason) {
                        error!(
                            "錯誤 KRS-E1303：簽發 CA {} 的私鑰無法使用，不使用這把金鑰（{}）。線上簽發停用，核准的申請停在「已核准，待簽發」；既有主機照常連線。處理方式：chmod 600 {}、chmod 700 {}，擁有者為執行 Kairos 的帳號",
                            ca.ca_id,
                            reason,
                            self.data_dir.join(key_file).display(),
                            pki::pki_dir(&self.data_dir).display()
                        );
                        logged.insert(ca.ca_id, reason.clone());
                    }
                    if ca.state == CA_ACTIVE {
                        problem = Some(reason);
                    }
                }
            }
        }
        let status = match (self.mode, active) {
            (PkiMode::Offline, _) => IssueStatus::Offline,
            (PkiMode::Online, None) => IssueStatus::Unavailable(
                "沒有啟用中的簽發 CA（kairos pki issuing-ca new → 以根 CA 簽發 → install）".into(),
            ),
            (PkiMode::Online, Some(id)) => match problem {
                Some(p) => IssueStatus::Unavailable(p),
                None if issuers.contains_key(&id) => IssueStatus::Online { ca_id: id },
                None => IssueStatus::Unavailable("啟用中的簽發 CA 沒有可用的私鑰".into()),
            },
        };
        let previous = self.status();
        if previous != status {
            match &status {
                IssueStatus::Online { ca_id } => info!(
                    "線上簽發已啟用：簽發 CA {}。警告 KRS-W1301：簽發 CA 私鑰存放在伺服器上（自動簽發的前提；被入侵時可簽出任意 agent_id 的用戶端憑證，但簽不出 Pulse 會信任的伺服器憑證）。不願在線上持有 CA 私鑰時改用 pki.mode = \"offline\"",
                    ca_id
                ),
                IssueStatus::Offline => {
                    info!(
                        "離線簽發模式：Kairos 不持有 CA 私鑰，核准的申請以 kairos pki queue export／import 簽發"
                    )
                }
                IssueStatus::Unavailable(r) => {
                    error!(
                        "錯誤 KRS-E1303：線上簽發無法使用：{r}；核准的申請停在「已核准，待簽發」"
                    )
                }
            }
        }
        *self.issuers.write().unwrap_or_else(|e| e.into_inner()) = issuers;
        *self.status.write().unwrap_or_else(|e| e.into_inner()) = status;

        // 信任錨：狀態為 active／trusted 且已安裝憑證的簽發 CA
        let anchors: Vec<(i64, String)> = cas
            .iter()
            .filter(|c| (c.state == CA_ACTIVE || c.state == CA_TRUSTED) && c.cert_der.is_some())
            .map(|c| (c.ca_id, c.state.clone()))
            .collect();
        let mut last = self.anchors.lock().unwrap_or_else(|e| e.into_inner());
        let path = pki::issuing_cas_path(&self.data_dir);
        if last.as_ref() == Some(&anchors) && (anchors.is_empty() || path.exists()) {
            return Ok(false);
        }
        let mut pem = String::new();
        for c in cas
            .iter()
            .filter(|c| anchors.iter().any(|(id, _)| *id == c.ca_id))
        {
            if let Some(der) = &c.cert_der {
                pem.push_str(&pem_encode("CERTIFICATE", der));
            }
        }
        if let Err(e) = pki::ensure_pki_dir(&self.data_dir)
            .and_then(|_| pki::write_public_atomically(&path, pem.as_bytes()))
        {
            error!("無法寫入簽發 CA 的信任錨 {}：{e}", path.display());
            return Ok(false);
        }
        let changed = last.is_some() || !anchors.is_empty();
        *last = Some(anchors);
        Ok(changed)
    }

    fn load_key(&self, ca_id: i64, cert_der: &[u8], key_file: &str) -> Result<IssuingCa, String> {
        let path = self.data_dir.join(key_file);
        if let Some(problem) = pki::key_permission_problem(&path) {
            return Err(problem);
        }
        let pem = std::fs::read_to_string(&path)
            .map_err(|e| format!("無法讀取 {}：{e}", path.display()))?;
        IssuingCa::load(ca_id, cert_der, &pem)
    }

    /// 簽發一筆申請（冪等：同一筆申請只會有一張憑證）。`decided_by` 為自動核准時的決定者
    /// （`auto`、`system`）；已由管理者核准的申請沿用核准者
    pub fn issue(
        &self,
        conn: &mut Connection,
        request_id: &str,
        decided_by: &str,
        eff: &Effective,
        now_ms: i64,
    ) -> Result<IssueOutcome, IssueError> {
        let issuer = self.active_issuer()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(req) = es::get_request(&tx, request_id)? else {
            return Ok(IssueOutcome::NotFound);
        };
        if req.state == DbState::Issued {
            let cert = req
                .issued_serial
                .as_deref()
                .map(|s| tx.get_cert(s))
                .transpose()?
                .flatten();
            return Ok(match cert {
                Some(c) => IssueOutcome::Already(c),
                None => IssueOutcome::NotIssuable(DbState::Issued),
            });
        }
        if !req.state.is_pending() {
            return Ok(IssueOutcome::NotIssuable(req.state));
        }
        let checked = match check_csr(&req.csr_der, &req.agent_id) {
            Ok(c) => c,
            Err(e) => {
                es::set_issue_error(&tx, request_id, &e)?;
                tx.commit()?;
                return Err(IssueError::Failed(e));
            }
        };
        let lifetime = eff.cert_lifetime_ms(req.cert_lifetime_days);
        let mut issued = None;
        for _ in 0..5 {
            let serial = random_serial().map_err(IssueError::Failed)?;
            let hex = crate::pki::x509::serial_hex(&serial);
            if tx.serial_exists(&hex)? {
                continue;
            }
            issued = Some(
                issuer
                    .sign_client(&checked.spki_der, &req.agent_id, now_ms, lifetime, serial)
                    .map_err(IssueError::Failed)?,
            );
            break;
        }
        let Some(issued) = issued else {
            return Err(IssueError::Failed("無法產生不重複的序號".into()));
        };
        let by = req
            .decided_by
            .clone()
            .unwrap_or_else(|| decided_by.to_string());
        let cert = CertRow {
            serial: issued.serial.clone(),
            ca_id: Some(issuer.ca_id),
            agent_id: req.agent_id.clone(),
            spki_sha256: checked.spki_sha256.to_vec(),
            not_before: issued.not_before_ms,
            not_after: issued.not_after_ms,
            cert_der: issued.der,
            issued_at: now_ms,
            issued_via: issued_via(req.kind).to_string(),
            request_id: Some(req.request_id.clone()),
            approved_by: Some(by.clone()),
            ..CertRow::default()
        };
        tx.insert_cert(&cert)?;
        es::mark_issued(&tx, request_id, &cert.serial, &by, now_ms)?;
        es::upsert_host_on_issue(
            &tx,
            &req.agent_id,
            &req.hostname,
            req.host_fingerprint.as_deref(),
            req.display_name.as_deref(),
            req.tags
                .as_ref()
                .map(|t| serde_json::Value::from(t.clone()).to_string())
                .as_deref(),
            req.os.as_deref(),
            &cert.serial,
            now_ms,
        )?;
        es::set_host_group_from_token(&tx, &req.agent_id, req.token_id.as_deref())?;
        es::audit(
            &tx,
            now_ms,
            &Audit {
                actor: &by,
                category: "pki",
                action: "cert.issue",
                object: Some(&cert.serial),
                result: "ok",
                details: Some(json!({
                    "request_id": req.request_id,
                    "agent_id": req.agent_id,
                    "kind": req.kind.as_str(),
                    "ca_id": issuer.ca_id,
                    "not_after": crate::api::http::ts_ms(cert.not_after),
                })),
                ..Audit::default()
            },
        )?;
        tx.commit()?;
        Ok(IssueOutcome::Issued(cert))
    }

    /// 重新產生一個簽發 CA 的 CRL（需持有私鑰）；回傳新的 CRL 版本
    pub fn regenerate_crl(
        &self,
        conn: &mut Connection,
        ca_id: i64,
        validity_ms: i64,
        source: &str,
        by: Option<&str>,
        now_ms: i64,
    ) -> Result<Option<i64>, IssueError> {
        let Some(issuer) = self.issuer(ca_id) else {
            return Ok(None);
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(ca) = tx.get_ca(ca_id)? else {
            return Ok(None);
        };
        let number = ca.next_crl_number.max(1);
        let revoked = tx.revocations_for_crl(ca_id, now_ms)?;
        let next_update = (now_ms + validity_ms).min(issuer.info.not_after_ms);
        let der = issuer
            .sign_crl(
                number as u64,
                now_ms,
                next_update.max(now_ms + 60_000),
                &revoked,
            )
            .map_err(IssueError::Failed)?;
        tx.record_crl(
            ca_id,
            number,
            now_ms,
            next_update,
            &revoked,
            &der,
            source,
            by,
        )?;
        tx.commit()?;
        Ok(Some(number))
    }

    /// 把各信任錨目前的 CRL 寫入 `pki/crl/issuing.crl`；內容有變化時回傳 `true`
    pub fn write_crl_file(&self, conn: &Connection) -> Result<bool, DbError> {
        let mut pem = String::new();
        for ca in conn.trust_anchor_cas()? {
            if let Some(crl) = conn.latest_crl(ca.ca_id)? {
                pem.push_str(&pem_encode("X509 CRL", &crl.der));
            }
        }
        let path = pki::issuing_crl_path(&self.data_dir);
        let current = std::fs::read_to_string(&path).ok();
        if current.as_deref() == Some(pem.as_str()) || (current.is_none() && pem.is_empty()) {
            return Ok(false);
        }
        if let Err(e) = pki::ensure_pki_dir(&self.data_dir)
            .and_then(|_| pki::write_public_atomically(&path, pem.as_bytes()))
        {
            error!("錯誤 KRS-E1205：無法寫入 CRL {}：{e}", path.display());
            return Ok(false);
        }
        Ok(true)
    }

    /// 持有私鑰的簽發 CA
    pub fn issuer_ids(&self) -> Vec<i64> {
        self.issuers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect()
    }
}

/// `pki_certificates.issued_via`
pub fn issued_via(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Enroll => "token",
        DbKind::GraceRenewal => "grace_renewal",
        DbKind::Renewal => "renewal",
    }
}

/// [`issue_approved`] 的結果：(簽發的申請, 失敗的 (request_id, 原因))
pub type IssuedBatch = (Vec<RequestRow>, Vec<(String, String)>);

/// 把待簽發的申請（核准但尚未簽發）簽發：常駐行程每 2 秒（CLI 核准的申請）與核准後立即呼叫。
/// 回傳 (簽發的 request_id, 失敗的 (request_id, 原因))
pub fn issue_approved(
    mgr: &PkiManager,
    conn: &mut Connection,
    eff: &Effective,
    now_ms: i64,
    limit: usize,
) -> Result<IssuedBatch, DbError> {
    if mgr.active_issuer().is_err() {
        return Ok((Vec::new(), Vec::new()));
    }
    let pending = es::approved_requests(conn, limit)?;
    let mut issued = Vec::new();
    let mut failed = Vec::new();
    for req in pending {
        // 先前失敗過的（CSR 不合法等）不再重試，留給管理者處理
        if req.issue_error.is_some() {
            continue;
        }
        match mgr.issue(conn, &req.request_id, "system", eff, now_ms) {
            Ok(IssueOutcome::Issued(_)) => issued.push(req),
            Ok(_) => {}
            Err(IssueError::Db(e)) => return Err(e),
            Err(IssueError::Unavailable(r)) | Err(IssueError::Failed(r)) => {
                failed.push((req.request_id.clone(), r))
            }
        }
    }
    Ok((issued, failed))
}
