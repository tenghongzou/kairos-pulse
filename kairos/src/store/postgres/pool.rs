//! PostgreSQL 的連線與連線池（`kairos/HA.md` 第 4.2、4.3 節；第 21 節 #4：自行實作的小型池）。
//!
//! - [`PgPool::get`]：取出一條連線（[`PgConn`]），drop 時歸還。池中的連線數最多 `storage.postgres.pool_size`，
//!   都在使用中時等待（最多 `connect_timeout_secs`），等不到回 `ResourceBusy`。
//! - [`PgPool::dedicated`]：不經過池的專用連線（report 的 writer：有自己的 `statement_timeout`，長期持有），
//!   不佔池的額度。
//! - TLS 用 rustls＋ring（`tokio-postgres-rustls`）；`verify-full` 以 `ca_file`（未設定時為作業系統的信任清單）
//!   驗證憑證與主機名稱，`require` 只加密不驗證。
//! - 密碼只從 `password_file` 讀取，每次建立連線時重新讀（輪替密碼不需要重新啟動）；不寫入日誌，
//!   錯誤訊息只有檔案路徑。

use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio_rustls::rustls;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};

use super::conn::{LockKind, PgConn, RawConn};
use crate::config::{PgSslMode, PostgresSettings};

/// 無法連線到 PostgreSQL（`kairos/HA.md` 第 19 節）
pub const CODE_PG_UNREACHABLE: &str = "KRS-E4401";
/// PostgreSQL 不符合要求
pub const CODE_PG_REQUIREMENTS: &str = "KRS-E4402";
/// PostgreSQL 沒有同步複寫
pub const CODE_PG_NO_SYNC_REPLICATION: &str = "KRS-W4403";
/// 資料庫的 schema 比這個 Kairos 新
pub const CODE_PG_SCHEMA_NEWER: &str = "KRS-E4405";
/// 節點的時鐘與資料庫的時鐘相差超過 2 秒
pub const CODE_PG_CLOCK_SKEW: &str = "KRS-W8305";

/// 最低的 PostgreSQL 版本（`server_version_num`）
pub const MIN_SERVER_VERSION_NUM: i64 = 140_000;
/// 時差的警告門檻
pub const CLOCK_SKEW_WARN_MS: i64 = 2_000;
/// 互動寫入等待寫入鎖的預設上限
pub const DEFAULT_BUSY: Duration = Duration::from_secs(5);

/// 連線的用途
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// 唯讀（`default_transaction_read_only = on`；對應 SQLite 的唯讀連線）
    ReadOnly,
    ReadWrite,
}

#[derive(Debug)]
struct NoVerify {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn tls_config(
    settings: &PostgresSettings,
    mode: PgSslMode,
) -> Result<rustls::ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    if mode == PgSslMode::Require {
        return Ok(builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify { provider }))
            .with_no_client_auth());
    }
    let mut roots = rustls::RootCertStore::empty();
    let ca = settings.ca_file.trim();
    if ca.is_empty() {
        for c in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(c);
        }
        if roots.is_empty() {
            return Err(
                "storage.postgres.ca_file 未設定，作業系統的信任清單中也沒有可用的 CA".to_string(),
            );
        }
    } else {
        let certs = CertificateDer::pem_file_iter(ca)
            .and_then(|it| it.collect::<Result<Vec<_>, _>>())
            .map_err(|e| format!("無法讀取 storage.postgres.ca_file {ca}: {e}"))?;
        if certs.is_empty() {
            return Err(format!("storage.postgres.ca_file {ca} 中沒有憑證"));
        }
        for c in certs {
            roots
                .add(c)
                .map_err(|e| format!("storage.postgres.ca_file {ca} 的憑證無法使用: {e}"))?;
        }
    }
    Ok(builder.with_root_certificates(roots).with_no_client_auth())
}

/// 讀取密碼檔（去掉結尾的換行）。空路徑：沒有密碼
fn read_password(
    settings: &PostgresSettings,
) -> Result<Option<zeroize::Zeroizing<String>>, String> {
    let path = settings.password_file.trim();
    if path.is_empty() {
        return Ok(None);
    }
    let raw = zeroize::Zeroizing::new(
        std::fs::read_to_string(path)
            .map_err(|e| format!("無法讀取 storage.postgres.password_file {path}: {e}"))?,
    );
    let password = raw.trim_end_matches(['\r', '\n']);
    if password.is_empty() {
        return Err(format!("storage.postgres.password_file {path} 是空的"));
    }
    Ok(Some(zeroize::Zeroizing::new(password.to_string())))
}

#[derive(Default)]
struct PoolState {
    idle: Vec<RawConn>,
    /// 池中的連線數（閒置＋使用中＋正在建立）
    total: usize,
}

/// 連線池的計數（`/metrics`、系統健康）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStats {
    pub size: usize,
    pub idle: usize,
    pub busy: usize,
    /// 建立連線的次數與失敗次數
    pub connects: u64,
    pub connect_errors: u64,
    /// 等不到連線（池已滿）的次數
    pub exhausted: u64,
    /// 因死結或序列化失敗自動重試的交易數（`40P01`、`40001`）
    pub retries_deadlock: u64,
    pub retries_serialization: u64,
}

/// 伺服器的狀態（啟動時與 `--check-config`、健康檢查讀取）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub version: String,
    pub version_num: i64,
    pub encoding: String,
    pub synchronous_commit: String,
    pub synchronous_standby_names: String,
    pub max_connections: i64,
    pub in_recovery: bool,
    /// 節點的時鐘減資料庫的時鐘（毫秒）
    pub clock_skew_ms: i64,
}

impl ServerInfo {
    /// 已 `ack` 的資料在主庫故障時不會遺失：`synchronous_commit` 為 on／remote_write／remote_apply，
    /// 且有同步備庫（`kairos/HA.md` 第 4.3 節）
    pub fn synchronous_replication(&self) -> bool {
        matches!(
            self.synchronous_commit.as_str(),
            "on" | "remote_write" | "remote_apply"
        ) && !self.synchronous_standby_names.trim().is_empty()
    }

    /// 不符合要求（拒絕啟動，`KRS-E4402`）時的原因
    pub fn unmet_requirement(&self) -> Option<String> {
        if self.version_num < MIN_SERVER_VERSION_NUM {
            return Some(format!(
                "PostgreSQL 的版本是 {}，Kairos 需要 14 以上",
                self.version
            ));
        }
        if !self.encoding.eq_ignore_ascii_case("UTF8") {
            return Some(format!(
                "資料庫的編碼是 {}，Kairos 需要 UTF8（建立資料庫時指定 ENCODING 'UTF8'）",
                self.encoding
            ));
        }
        if self.in_recovery {
            return Some(
                "連線到的是唯讀的備庫（pg_is_in_recovery() 為 true）；storage.postgres.host 必須指向主庫".to_string(),
            );
        }
        None
    }
}

/// 一個 PostgreSQL 資料庫（與其中的一個 schema）的連線池
pub struct PgPool {
    settings: PostgresSettings,
    /// 節點本機的資料目錄（金鑰檔所在；測試與只有連線設定的工具為 `None`）
    data_dir: Option<std::path::PathBuf>,
    tls: OnceLock<Result<Option<tokio_postgres_rustls::MakeRustlsConnect>, String>>,
    state: Mutex<PoolState>,
    available: Condvar,
    connects: AtomicU64,
    connect_errors: AtomicU64,
    exhausted: AtomicU64,
    retries_deadlock: AtomicU64,
    retries_serialization: AtomicU64,
    /// 最近一次讀到的伺服器狀態（健康檢查每次更新；系統健康與 `/metrics` 讀取）
    server: Mutex<Option<ServerInfo>>,
    /// 上次記錄伺服器警告（`KRS-W4403`、`KRS-W8305`）的時間：啟動時與之後每小時一次
    warned_at: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for PgPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PgPool({})", self.settings.display_target())
    }
}

impl PgPool {
    /// 建立連線池（不連線；第一次 [`PgPool::get`] 時才建立連線）
    pub fn new(settings: PostgresSettings) -> Arc<Self> {
        Self::with_data_dir(settings, None)
    }

    /// [`PgPool::new`]，另指定節點本機的資料目錄（稽核 HMAC 金鑰等金鑰檔從這裡讀取）
    pub fn with_data_dir(
        settings: PostgresSettings,
        data_dir: Option<std::path::PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self {
            settings,
            data_dir,
            tls: OnceLock::new(),
            state: Mutex::new(PoolState::default()),
            available: Condvar::new(),
            connects: AtomicU64::new(0),
            connect_errors: AtomicU64::new(0),
            exhausted: AtomicU64::new(0),
            retries_deadlock: AtomicU64::new(0),
            retries_serialization: AtomicU64::new(0),
            server: Mutex::new(None),
            warned_at: Mutex::new(None),
        })
    }

    /// 記下最近一次讀到的伺服器狀態
    pub fn note_server(&self, info: &ServerInfo) {
        *self.server.lock().unwrap_or_else(|e| e.into_inner()) = Some(info.clone());
    }

    /// 最近一次讀到的伺服器狀態（還沒有連線過時為 `None`）
    pub fn last_server(&self) -> Option<ServerInfo> {
        self.server
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 伺服器警告是否該記錄了（第一次，或距離上次超過 `every`）；回傳 `true` 時同時記下時間
    pub fn warning_due(&self, every: Duration) -> bool {
        let mut at = self.warned_at.lock().unwrap_or_else(|e| e.into_inner());
        if at.is_some_and(|t| t.elapsed() < every) {
            return false;
        }
        *at = Some(Instant::now());
        true
    }

    pub fn settings(&self) -> &PostgresSettings {
        &self.settings
    }

    /// 對應 SQLite 資料庫檔路徑的字串（`<data_dir>/kairos.db`；檔案不存在）：以資料庫路徑找出資料目錄的
    /// 既有函式（例如 [`crate::audit::key_for_db_path`]）沿用同一個介面
    pub fn db_path_hint(&self) -> Option<String> {
        self.data_dir
            .as_ref()
            .map(|d| d.join(crate::store::sqlite::DB_FILE).display().to_string())
    }

    /// Kairos 的資料表所在的 schema
    pub fn schema(&self) -> &str {
        &self.settings.schema
    }

    /// advisory lock 的鍵：高 32 位元是「KRS1」，低 32 位元是 schema 名稱的雜湊與鎖的種類
    /// （同一個資料庫中不同 schema 的 Kairos 互不影響）
    pub fn lock_key(&self, kind: LockKind) -> i64 {
        let mut h: u32 = 0x811C_9DC5;
        for b in self.settings.schema.bytes() {
            h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
        }
        let low = h.wrapping_add(kind as u32);
        (i64::from(crate::store::sqlite::APPLICATION_ID) << 32) | i64::from(low)
    }

    fn connect(&self) -> Result<RawConn, Error> {
        super::conn::blocking(|| self.connect_blocking())
    }

    fn connect_blocking(&self) -> Result<RawConn, Error> {
        let s = &self.settings;
        let mode = s.effective_sslmode();
        let mut config = postgres::Config::new();
        config
            .port(s.port)
            .dbname(&s.dbname)
            .user(&s.user)
            .application_name("kairos")
            .connect_timeout(s.connect_timeout())
            // 伺服器或網路無聲無息地消失時，由 TCP keepalive 發現（預設即啟用；這裡縮短閒置時間）
            .keepalives_idle(Duration::from_secs(30))
            .ssl_mode(match mode {
                PgSslMode::Disable => postgres::config::SslMode::Disable,
                PgSslMode::Require | PgSslMode::VerifyFull => postgres::config::SslMode::Require,
            });
        if s.is_unix_socket() {
            #[cfg(unix)]
            config.host_path(&s.host);
            #[cfg(not(unix))]
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "這個平台不支援以 Unix socket 連線到 PostgreSQL",
            ));
        } else {
            config.host(s.host.trim().trim_start_matches('[').trim_end_matches(']'));
        }
        let password = read_password(s).map_err(|e| Error::new(ErrorKind::InvalidInput, e))?;
        if let Some(p) = &password {
            config.password(p.as_bytes());
        }
        self.connects.fetch_add(1, Ordering::Relaxed);
        let tls =
            self.tls
                .get_or_init(|| match mode {
                    PgSslMode::Disable => Ok(None),
                    m => tls_config(s, m)
                        .map(|c| Some(tokio_postgres_rustls::MakeRustlsConnect::new(c))),
                })
                .clone()
                .map_err(|e| Error::new(ErrorKind::InvalidInput, e))?;
        let connected = match tls {
            Some(tls) => config.connect(tls),
            None => config.connect(postgres::NoTls),
        };
        let mut client = connected.map_err(|e| {
            self.connect_errors.fetch_add(1, Ordering::Relaxed);
            let e = super::conn::PgError::from(e);
            Error::new(
                ErrorKind::ConnectionRefused,
                format!("無法連線到 PostgreSQL（{}）: {e}", s.display_target()),
            )
        })?;
        // schema 名稱已由設定驗證為不需要引號的識別字。schema 還不存在時 search_path 仍可設定
        // （遷移會建立它）。lock_timeout：一般敘述等待列鎖的上限，避免無限期等待
        client
            .batch_execute(&format!(
                "SET search_path = {}; SET lock_timeout = '30s'; SET client_min_messages = warning",
                s.schema
            ))
            .map_err(|e| {
                self.connect_errors.fetch_add(1, Ordering::Relaxed);
                Error::other(format!(
                    "PostgreSQL 連線的初始設定失敗: {}",
                    super::conn::PgError::from(e)
                ))
            })?;
        Ok(RawConn {
            client,
            stmts: HashMap::new(),
            read_only: false,
        })
    }

    fn prepare(
        self: &Arc<Self>,
        mut raw: RawConn,
        access: Access,
        busy: Duration,
    ) -> Result<PgConn, Error> {
        let want = access == Access::ReadOnly;
        if raw.read_only != want {
            let sql = if want {
                "SET default_transaction_read_only = on"
            } else {
                "SET default_transaction_read_only = off"
            };
            if let Err(e) = super::conn::blocking(|| raw.client.batch_execute(sql)) {
                self.discard();
                return Err(Error::other(format!(
                    "PostgreSQL 連線無法使用: {}",
                    super::conn::PgError::from(e)
                )));
            }
            raw.read_only = want;
        }
        Ok(PgConn::new(raw, self.clone(), busy, true))
    }

    /// 取出一條連線。`busy`：這條連線上 [`crate::store::db::TxMode::Immediate`] 等待寫入鎖的上限
    pub fn get(self: &Arc<Self>, access: Access, busy: Duration) -> Result<PgConn, Error> {
        let deadline = Instant::now() + self.settings.connect_timeout();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            // 優先取用途相同的閒置連線（不必切換 read_only）
            let want = access == Access::ReadOnly;
            let pick = state
                .idle
                .iter()
                .rposition(|c| c.read_only == want)
                .or_else(|| state.idle.len().checked_sub(1));
            if let Some(i) = pick {
                let raw = state.idle.swap_remove(i);
                if raw.client.is_closed() {
                    state.total = state.total.saturating_sub(1);
                    continue;
                }
                drop(state);
                return self.prepare(raw, access, busy);
            }
            if state.total < self.settings.pool_size {
                state.total += 1;
                drop(state);
                return match self.connect() {
                    Ok(raw) => self.prepare(raw, access, busy),
                    Err(e) => {
                        self.discard();
                        Err(e)
                    }
                };
            }
            let now = Instant::now();
            if now >= deadline {
                self.exhausted.fetch_add(1, Ordering::Relaxed);
                return Err(Error::new(
                    ErrorKind::ResourceBusy,
                    format!(
                        "PostgreSQL 連線池的 {} 條連線都在使用中（storage.postgres.pool_size）",
                        self.settings.pool_size
                    ),
                ));
            }
            state = self
                .available
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// 不經過池的專用讀寫連線（drop 時關閉）
    pub fn dedicated(self: &Arc<Self>, busy: Duration) -> Result<PgConn, Error> {
        Ok(PgConn::new(self.connect()?, self.clone(), busy, false))
    }

    /// 歸還一條可以繼續使用的連線
    pub(super) fn put_back(&self, raw: RawConn) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.idle.push(raw);
        drop(state);
        self.available.notify_one();
    }

    /// 一條池中的連線已關閉（中斷、建立失敗）
    pub(super) fn discard(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.total = state.total.saturating_sub(1);
        drop(state);
        self.available.notify_one();
    }

    /// 關閉所有閒置的連線（測試清理）
    #[cfg(test)]
    pub fn close_idle(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let idle = std::mem::take(&mut state.idle);
        state.total = state.total.saturating_sub(idle.len());
        drop(state);
        super::conn::blocking(|| drop(idle));
    }

    /// 記一次交易的自動重試
    pub fn count_retry(&self, sqlstate: Option<&str>) {
        match sqlstate {
            Some("40P01") => self.retries_deadlock.fetch_add(1, Ordering::Relaxed),
            _ => self.retries_serialization.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub fn stats(&self) -> PoolStats {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let idle = state.idle.len();
        PoolStats {
            size: self.settings.pool_size,
            idle,
            busy: state.total.saturating_sub(idle),
            connects: self.connects.load(Ordering::Relaxed),
            connect_errors: self.connect_errors.load(Ordering::Relaxed),
            exhausted: self.exhausted.load(Ordering::Relaxed),
            retries_deadlock: self.retries_deadlock.load(Ordering::Relaxed),
            retries_serialization: self.retries_serialization.load(Ordering::Relaxed),
        }
    }
}

/// 讀取伺服器的狀態（一個查詢）
pub fn server_info(conn: &PgConn) -> super::conn::PgResult<ServerInfo> {
    conn.query_row(
        "SELECT current_setting('server_version'),
                current_setting('server_version_num')::bigint,
                current_setting('server_encoding'),
                current_setting('synchronous_commit'),
                COALESCE(current_setting('synchronous_standby_names', true), ''),
                current_setting('max_connections')::bigint,
                pg_is_in_recovery(),
                (extract(epoch FROM clock_timestamp()) * 1000)::bigint",
        (),
        |r| {
            let db_now: i64 = r.get(7)?;
            Ok(ServerInfo {
                version: r.get(0)?,
                version_num: r.get(1)?,
                encoding: r.get(2)?,
                synchronous_commit: r.get(3)?,
                synchronous_standby_names: r.get(4)?,
                max_connections: r.get(5)?,
                in_recovery: r.get(6)?,
                clock_skew_ms: chrono::Utc::now().timestamp_millis() - db_now,
            })
        },
    )
}

// ---------------------------------------------------------------------------------------------
// 行程使用的後端
// ---------------------------------------------------------------------------------------------

static ACTIVE: Mutex<Option<Arc<PgPool>>> = Mutex::new(None);

/// 依設定決定這個行程使用的資料庫後端：`storage.backend = "postgres"` 時建立連線池（不連線），之後
/// [`crate::store::open_read`] 等函式回傳 PostgreSQL 的連線而不是開啟 `kairos.db`。設定相同時沿用既有的池
/// （重新載入設定不會中斷連線）；後端或連線設定的變更需要重新啟動
#[cfg_attr(test, allow(dead_code))]
pub fn configure(config: &crate::config::AppConfig) {
    let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    if config.storage.backend != crate::config::StorageBackend::Postgres {
        return;
    }
    match active.as_ref() {
        Some(pool) if pool.settings == config.storage.postgres => {}
        Some(_) => log::warn!(
            "storage.postgres 的設定已變更，重新啟動後才會生效（目前的連線沿用啟動時的設定）"
        ),
        None => {
            *active = Some(PgPool::with_data_dir(
                config.storage.postgres.clone(),
                Some(std::path::PathBuf::from(&config.storage.data_dir)),
            ));
        }
    }
}

/// 這個行程使用的 PostgreSQL 連線池（`storage.backend` 不是 `postgres` 時為 `None`）
pub fn active() -> Option<Arc<PgPool>> {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 測試輔助：指定（或清除）這個行程使用的連線池
#[cfg(test)]
#[allow(dead_code)]
pub fn set_active(pool: Option<Arc<PgPool>>) {
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = pool;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> ServerInfo {
        ServerInfo {
            version: "16.4".into(),
            version_num: 160_004,
            encoding: "UTF8".into(),
            synchronous_commit: "on".into(),
            synchronous_standby_names: String::new(),
            max_connections: 100,
            in_recovery: false,
            clock_skew_ms: 0,
        }
    }

    #[test]
    fn requirements_follow_the_spec() {
        assert_eq!(info().unmet_requirement(), None);
        let old = ServerInfo {
            version: "13.9".into(),
            version_num: 130_009,
            ..info()
        };
        assert!(old.unmet_requirement().unwrap().contains("14 以上"));
        let latin = ServerInfo {
            encoding: "LATIN1".into(),
            ..info()
        };
        assert!(latin.unmet_requirement().unwrap().contains("UTF8"));
        let standby = ServerInfo {
            in_recovery: true,
            ..info()
        };
        assert!(standby.unmet_requirement().unwrap().contains("備庫"));
    }

    #[test]
    fn synchronous_replication_needs_a_standby() {
        assert!(!info().synchronous_replication());
        let synced = ServerInfo {
            synchronous_standby_names: "ANY 1 (pg2, pg3)".into(),
            ..info()
        };
        assert!(synced.synchronous_replication());
        let off = ServerInfo {
            synchronous_commit: "off".into(),
            ..synced.clone()
        };
        assert!(!off.synchronous_replication());
        let local = ServerInfo {
            synchronous_commit: "local".into(),
            ..synced
        };
        assert!(!local.synchronous_replication());
    }

    #[test]
    fn lock_keys_differ_by_schema_and_kind() {
        let a = PgPool::new(PostgresSettings::default());
        let b = PgPool::new(PostgresSettings {
            schema: "kairos_b".into(),
            ..PostgresSettings::default()
        });
        assert_ne!(a.lock_key(LockKind::Write), a.lock_key(LockKind::Migrate));
        assert_ne!(a.lock_key(LockKind::Write), b.lock_key(LockKind::Write));
        assert_eq!(a.lock_key(LockKind::Write) >> 32, 0x4B52_5331);
    }

    #[test]
    fn password_file_is_trimmed_and_errors_do_not_leak_content() {
        let dir = std::env::temp_dir().join(format!("kairos-pgpw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pg.password");
        std::fs::write(&path, "s3cret\r\n").unwrap();
        let mut s = PostgresSettings {
            password_file: path.display().to_string(),
            ..PostgresSettings::default()
        };
        assert_eq!(read_password(&s).unwrap().unwrap().as_str(), "s3cret");
        std::fs::write(&path, "\n").unwrap();
        assert!(read_password(&s).unwrap_err().contains("是空的"));
        s.password_file = dir.join("missing").display().to_string();
        let e = read_password(&s).unwrap_err();
        assert!(e.contains("password_file") && !e.contains("s3cret"));
        s.password_file = String::new();
        assert!(read_password(&s).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
