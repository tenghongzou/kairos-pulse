//! 註冊通道（8444）用戶端：`pulse enroll`、服務的待核准輪詢、過期寬限期續簽與撤回申請。
//!
//! 規格只有一份：`crates/kairos-proto/ENROLL.md`；設計依據為設計包 `pulse/enrollment-ux.md`。
//! 共用型別、簽章內容（canonical bytes）、token 與安裝字串都來自 `kairos_proto::enroll`，
//! 這裡只有 Pulse 端的 I/O 與狀態機：
//!
//! | 模組 | 內容 |
//! |---|---|
//! | [`trust`] | 8444 只信任 CA 指紋、`ca.pem` 或 TOFU（不使用作業系統信任庫） |
//! | [`http`]、[`client`] | HTTP/1.1 keep-alive 用戶端、逾時、回應與錯誤本文的解析 |
//! | [`keys`] | P-256 私鑰（本機產生、不離開主機）、CSR、`poll_secret` |
//! | [`store`] | `identity/` 的檔案、原子寫入、權限、`enrollment.json`、檔案鎖 |
//! | [`schedule`]、[`machine`] | 重送、輪詢、退避與抖動；回應 → Pulse 端狀態（純函式） |
//! | [`flow`] | 送出 → 輪詢 → 安裝；過期續簽；撤回 |
//! | [`cli`] | `pulse enroll` |
//! | [`service`] | 服務啟動時的註冊閘門、背景輪詢、過期續簽 |

pub mod cli;
pub mod client;
pub mod flow;
pub mod http;
pub mod keys;
pub mod machine;
pub mod schedule;
pub mod service;
pub mod store;
pub mod toml_edit;
pub mod trust;
pub mod x509;

#[cfg(test)]
pub mod fake;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub mod vectors;

use kairos_proto::enroll::{self as ke, EnrollToken, ServerVerification};

use self::flow::RequestInfo;
use self::store::{EnrollmentFile, FileKind, IdentityPaths, state};

/// 新註冊申請的內容
#[derive(Debug, Clone)]
pub struct NewEnrollment {
    pub agent_id: String,
    /// agent 連線位址（8443，正規寫法）
    pub server: Option<String>,
    /// 註冊端點（正規寫法）
    pub enroll_server: String,
    pub token: EnrollToken,
    pub verification: ServerVerification,
    /// `ca_pin`／`ca_file`／`tofu_interactive`／`tofu_flag`
    pub verified_by: String,
    pub ca_pin: Option<String>,
    pub trusted_pins: Vec<String>,
    pub info: RequestInfo,
    pub previous_serial: Option<String>,
}

/// 準備新的申請：產生私鑰（`request.key`，0600）、CSR、`poll_secret`，寫入 `enrollment.json`
/// （`state = submitting`）。**送出前**就寫入，回應遺失或中斷後以同一組重送。
pub fn prepare(paths: &IdentityPaths, n: &NewEnrollment) -> Result<EnrollmentFile, String> {
    paths
        .ensure_dir()
        .map_err(|e| format!("無法建立 {}：{e}", paths.dir.display()))?;
    let pkcs8 = keys::generate_p256_pkcs8()?;
    let pem = keys::pkcs8_pem(&pkcs8)?;
    store::write_atomic(&paths.request_key(), pem.as_bytes(), FileKind::Secret)
        .map_err(|e| format!("無法寫入私鑰 {}：{e}", paths.request_key().display()))?;
    let csr = keys::build_csr(&pkcs8, &n.agent_id)?;
    let file = EnrollmentFile {
        kind: "enroll".into(),
        state: state::SUBMITTING.into(),
        agent_id: n.agent_id.to_ascii_lowercase(),
        server: n.server.clone(),
        enroll_server: n.enroll_server.clone(),
        server_verification: Some(n.verification),
        server_verified_by: Some(n.verified_by.clone()),
        ca_pin: n.ca_pin.clone(),
        trusted_ca_pins: n.trusted_pins.clone(),
        token: Some(n.token.canonical()),
        token_prefix: Some(n.token.prefix4()),
        csr: Some(ke::b64url_encode(&csr)),
        poll_secret: Some(keys::new_poll_secret()?),
        hostname: Some(n.info.hostname.clone()),
        display_name: n.info.display_name.clone(),
        tags: n.info.tags.clone(),
        host_fingerprint: n.info.host_fingerprint.clone(),
        os: n.info.os.clone(),
        previous_serial: n.previous_serial.clone(),
        ..Default::default()
    };
    paths
        .save_enrollment(&file)
        .map_err(|e| format!("無法寫入 {}：{e}", paths.enrollment().display()))?;
    Ok(file)
}

/// 由 `host` 與連接埠組出正規寫法的端點（IPv6 加上 `[]`）
pub fn endpoint_of(host: &str, port: u16) -> Result<String, String> {
    let host = host.trim();
    let raw = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    ke::normalize_endpoint(&raw).map_err(|e| format!("「{raw}」不是合法的端點：{e}"))
}

/// 設定中的註冊端點：`[enroll] address`（空字串時與 `server.address` 相同主機）＋ `[enroll] port`
/// （設計包 `decisions/naming.md` §4.2 的推導規則）
pub fn configured_enroll_endpoint(
    config: &crate::config_manager::AppConfig,
) -> Result<String, String> {
    let host = match config.enroll.address.trim() {
        "" => config.server.address.trim(),
        h => h,
    };
    endpoint_of(host, config.enroll.port)
}

/// `identity.dir`（相對路徑以 `base` 解析）
pub fn identity_paths(
    config: &crate::config_manager::AppConfig,
    base: &std::path::Path,
) -> IdentityPaths {
    IdentityPaths::new(base.join(config.identity.dir.trim()))
}
