//! 代理簽發 CA 的建立與安裝（`kairos/PKI.md` 第 4 節；`kairos pki issuing-ca new|install|activate`）。
//!
//! 流程（根 CA 從頭到尾不在 Kairos 主機上）：
//! 1. `new`：在 `data/pki/` 產生 P-256 私鑰（0600）與 CSR，資料庫記一筆 `pending` 的 CA。
//! 2. 管理者在離線主機以根 CA 簽發（`basicConstraints=critical,CA:TRUE,pathlen:0`、
//!    `keyUsage=critical,keyCertSign,cRLSign`、`extendedKeyUsage=clientAuth`）。
//! 3. `install`：檢查憑證與私鑰成對、擴充欄位、有效期間（至少剩 2 年），狀態改為 `trusted`；
//!    沒有啟用中的 CA 時同時啟用（第一個簽發 CA）。

use std::path::Path;

use super::issue::{
    IssuingCa, MIN_ISSUING_CA_REMAINING_MS, NewIssuingCa, check_issuing_ca, new_issuing_ca,
};
use super::store::{CA_PENDING, CA_RETIRED};
use super::x509::{parse_cert, pem_decode_all};
use crate::store::prelude::*;

/// 建立新的簽發 CA：寫入私鑰與 `pending` 的資料列，回傳 `ca_id` 與 CSR
pub fn create(
    conn: &dyn Store,
    data_dir: &Path,
    organization: &str,
    now_ms: i64,
) -> Result<(i64, NewIssuingCa), String> {
    let next = conn.next_ca_id().map_err(|e| e.to_string())?;
    let n = new_issuing_ca(organization, next)?;
    super::ensure_pki_dir(data_dir)
        .map_err(|e| format!("無法建立 {}：{e}", super::pki_dir(data_dir).display()))?;
    let key_file = super::key_file_name(next);
    let key_path = data_dir.join(&key_file);
    super::write_private_key(&key_path, &n.key_pem)
        .map_err(|e| format!("無法寫入私鑰 {}：{e}", key_path.display()))?;
    let spki_sha = kairos_proto::enroll::sha256(&n.spki_der);
    let subject = n.subject.clone();
    let id = conn.insert_pending_ca(
        &|_| subject.clone(),
        &spki_sha,
        &|_| key_file.clone(),
        now_ms,
    );
    match id {
        Ok(id) if id == next => Ok((id, n)),
        Ok(id) => {
            // 並行建立時編號不同：私鑰檔名依資料庫的編號重新命名
            let to = data_dir.join(super::key_file_name(id));
            std::fs::rename(&key_path, &to).map_err(|e| e.to_string())?;
            conn.set_ca_key_file(id, &super::key_file_name(id))
                .map_err(|e| e.to_string())?;
            Ok((id, n))
        }
        Err(e) => {
            let _ = std::fs::remove_file(&key_path);
            Err(e.to_string())
        }
    }
}

/// 安裝結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub ca_id: i64,
    pub subject: String,
    /// 簽發者（根 CA）的 subject
    pub issuer: String,
    pub not_after_ms: i64,
    pub activated: bool,
}

/// 安裝根 CA 簽發的簽發 CA 憑證（PEM；檔案中第一張憑證）
pub fn install(
    conn: &dyn Store,
    data_dir: &Path,
    cert_pem: &str,
    now_ms: i64,
) -> Result<Installed, String> {
    let blocks = pem_decode_all(cert_pem, Some("CERTIFICATE"))?;
    let Some((_, der)) = blocks.into_iter().next() else {
        return Err("檔案中找不到憑證（PEM 的 CERTIFICATE）".into());
    };
    let info = parse_cert(&der)?;
    let spki_sha = kairos_proto::enroll::sha256(&info.spki_der);
    let ca = conn
        .find_ca_by_spki(&spki_sha)
        .map_err(|e| e.to_string())?
        .ok_or("這張憑證的公鑰不屬於任何以 kairos pki issuing-ca new 建立的簽發 CA")?;
    if ca.state == CA_RETIRED {
        return Err(format!("簽發 CA {} 已退役，不能再安裝", ca.ca_id));
    }
    if ca.state != CA_PENDING && ca.cert_der.as_deref() == Some(der.as_slice()) {
        return Err(format!("簽發 CA {} 已安裝過這張憑證", ca.ca_id));
    }
    check_issuing_ca(&info)?;
    if info.not_after_ms - now_ms < MIN_ISSUING_CA_REMAINING_MS {
        return Err("簽發 CA 憑證的有效期間至少要剩 2 年".into());
    }
    if info.not_before_ms > now_ms + 600_000 {
        return Err("簽發 CA 憑證尚未生效（notBefore 在未來）".into());
    }
    let key_file = ca
        .key_file
        .clone()
        .ok_or("這個簽發 CA 沒有私鑰檔（離線建立的 CA 不能安裝在 Kairos 上）")?;
    let key_pem = std::fs::read_to_string(data_dir.join(&key_file))
        .map_err(|e| format!("無法讀取私鑰 {key_file}：{e}"))?;
    IssuingCa::load(ca.ca_id, &der, &key_pem)?;
    let activated = conn
        .install_ca(
            ca.ca_id,
            &info.subject_text,
            &der,
            info.not_before_ms,
            info.not_after_ms,
        )
        .map_err(|e| e.to_string())?;
    Ok(Installed {
        ca_id: ca.ca_id,
        issuer: info.issuer_text,
        subject: info.subject_text,
        not_after_ms: info.not_after_ms,
        activated,
    })
}
