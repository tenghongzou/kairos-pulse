mod auth;
mod config;
mod connection;
mod fsperm;
#[cfg(test)]
mod integration_tests;
mod logger;
mod monitor;
mod per_ip;
mod protocol;
mod rate;
mod registry;
mod reload;
mod retention;
mod shutdown;
mod storage;
#[cfg(test)]
mod test_pki;
mod throttle;
mod tls;

use log::{debug, error, info, warn};
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{MissedTickBehavior, interval, sleep, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::auth::BindingPolicy;
use crate::config::{AppConfig, CliAction, ListenAuth, USAGE, parse_args};
use crate::connection::{ConnContext, handle_connection, is_peer_disconnect};
use crate::per_ip::{PerIpLimiter, Rejected};
use crate::rate::ReportLimiter;
use crate::registry::{Registry, next_conn_id};
use crate::shutdown::{CONNECTIONS_DEADLINE, MONITOR_DEADLINE, Shutdown, WRITER_DEADLINE};
use crate::throttle::{Category, LogThrottle};
use crate::tls::{TlsPaths, TlsState};

/// 版本字串（`--version` 與啟動日誌第一行；D3）：`kairos <版本> (<commit>)`。
/// commit 由 build.rs 決定：環境變數 `BUILD_COMMIT`、`git rev-parse --short HEAD`，都失敗時為 `unknown`
pub const VERSION: &str = concat!(
    "kairos ",
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("KAIROS_BUILD_COMMIT"),
    ")"
);

/// accept 因資源問題失敗（例如檔案描述符用盡）後的等待時間
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// 等待新連線期間檢查「是否已解除飽和」的間隔
const SATURATION_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// `block_on` 返回後，等待 blocking 執行緒（例如卡住的 fsync）結束的上限；
/// 超過就不再等待，讓行程以 `run` 的結果結束（R2-K17）
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);

/// 除了連線之外，行程需要的檔案描述符（日誌、資料檔、TLS 設定檔、listener 等）的估計量
const FD_RESERVE: u64 = 64;

/// 「握手中」（已 accept、尚未通過 hello）連線數的上限：`max_connections` 的一半（至少 1）。
/// 其餘名額保留給已通過 hello 的連線，慢速握手無法佔滿全域名額（R2-K2）
pub fn handshake_limit(max_connections: usize) -> usize {
    (max_connections / 2).max(1)
}

/// 將 std 的 Instant 轉成 tokio 的 Instant，供 `timeout_at` 使用
fn tokio_deadline(at: Instant) -> tokio::time::Instant {
    tokio::time::Instant::from_std(at)
}

/// accept 失敗是否只與該條連線有關（對方在 accept 前就重設或中止連線、被訊號中斷等）。
/// 這類錯誤直接接受下一條連線；其他錯誤（例如檔案描述符用盡）才等待後重試（R2-K14）
fn accept_error_is_per_connection(e: &Error) -> bool {
    is_peer_disconnect(e)
        || matches!(
            e.kind(),
            ErrorKind::ConnectionRefused
                | ErrorKind::Interrupted
                | ErrorKind::WouldBlock
                | ErrorKind::TimedOut
        )
}

/// 監看應持續執行到關機的背景工作（report writer、monitor、TLS 重載、日誌清除）。
/// 在關機前結束（不論 panic 或正常返回）都是異常：記 ERROR 並觸發關機，
/// 而不是等到關機時才發現。關機期間的結束屬正常，只有 panic 才記錄。
fn supervise(name: &'static str, task: JoinHandle<()>, shutdown: Arc<Shutdown>) -> JoinHandle<()> {
    supervise_with(name, task, shutdown, shutdown::fatal)
}

/// [`supervise`] 的實作；`on_fatal` 可替換，方便測試（正式程式會啟動關機看門狗）
fn supervise_with(
    name: &'static str,
    task: JoinHandle<()>,
    shutdown: Arc<Shutdown>,
    on_fatal: fn(&Shutdown, String),
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = task.await;
        let shutting_down = shutdown.started_at().is_some();
        let reason = match result {
            Ok(()) if shutting_down => return,
            Ok(()) => format!("{}意外結束", name),
            Err(e) => format!("{}異常結束: {}", name, e),
        };
        if shutting_down {
            error!("{}", reason);
        } else {
            on_fatal(&shutdown, reason);
        }
    })
}

/// 連線上限的飽和狀態。只在進入與解除飽和時各回報一次，避免日誌洗版。
/// 解除條件帶遲滯：可用名額回到上限的 10%（至少 1）才算解除，避免在臨界點反覆切換。
#[derive(Debug)]
struct Saturation {
    saturated: bool,
    resume_at: usize,
}

impl Saturation {
    fn new(max_connections: usize) -> Self {
        Self {
            saturated: false,
            resume_at: (max_connections / 10).max(1),
        }
    }

    /// 取不到名額時呼叫；回傳 `true` 表示剛進入飽和
    fn enter(&mut self) -> bool {
        !std::mem::replace(&mut self.saturated, true)
    }

    /// 以目前可用名額檢查；回傳 `true` 表示剛解除飽和
    fn check_resume(&mut self, available: usize) -> bool {
        if self.saturated && available >= self.resume_at {
            self.saturated = false;
            true
        } else {
            false
        }
    }
}

/// accept 迴圈：先取得全域連線名額再 accept，再取得來源的名額（同時連線數與新連線速率）與
/// 「握手中」的名額，為每條連線啟動 [`handle_connection`]，直到關機。
/// 回傳仍在執行的連線（listener 隨之釋放，不再接受新連線）。
pub(crate) async fn accept_loop(
    listener: TcpListener,
    ctx: Arc<ConnContext>,
    max_connections: usize,
    per_ip: Arc<PerIpLimiter>,
) -> Result<JoinSet<()>, Error> {
    let shutdown = ctx.shutdown.clone();
    let throttle = ctx.throttle.clone();
    let limit = Arc::new(Semaphore::new(max_connections));
    let max_handshakes = handshake_limit(max_connections);
    let handshakes = Arc::new(Semaphore::new(max_handshakes));
    let mut connections = JoinSet::new();
    let mut saturation = Saturation::new(max_connections);
    let mut saturation_check = interval(SATURATION_CHECK_INTERVAL);
    saturation_check.set_missed_tick_behavior(MissedTickBehavior::Delay);
    'accept: loop {
        // 回收已結束的連線工作
        while let Some(res) = connections.try_join_next() {
            if let Err(e) = res
                && e.is_panic()
            {
                error!("連線工作異常結束: {}", e);
            }
        }
        // 先取得連線名額再 accept；名額用盡時暫停接受新連線
        let permit: OwnedSemaphorePermit = match limit.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                if saturation.enter() {
                    warn!("已達連線上限 {}，暫停接受新連線", max_connections);
                }
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break 'accept,
                    res = limit.clone().acquire_owned() => {
                        res.map_err(|_| Error::other("連線上限的 Semaphore 已關閉"))?
                    }
                }
            }
            Err(TryAcquireError::Closed) => {
                return Err(Error::other("連線上限的 Semaphore 已關閉"));
            }
        };

        // 等待新連線；期間定期檢查是否已解除飽和，名額釋放後即時記錄，而不是等到下一條連線
        let accepted = loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break 'accept,
                res = listener.accept() => break res,
                _ = saturation_check.tick() => {
                    // 手上這個名額尚未使用，也算可用
                    if saturation.check_resume(limit.available_permits() + 1) {
                        info!(
                            "連線數已降至上限 {} 以下，恢復接受新連線",
                            max_connections
                        );
                    }
                }
            }
        };
        let (socket, peer) = match accepted {
            Ok(conn) => conn,
            // 只與該條連線有關的錯誤：直接接受下一條，不等待
            Err(e) if accept_error_is_per_connection(&e) => {
                debug!("接受連線失敗（只影響該條連線）: {}", e);
                drop(permit);
                continue;
            }
            Err(e) => {
                // 資源問題（例如檔案描述符用盡）不應讓整個伺服器結束，稍候再重試；
                // 可能每 100 ms 發生一次，節流記錄。等待期間仍可關機
                throttled!(throttle, Category::Accept, error, "接受連線失敗: {}", e);
                drop(permit);
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break 'accept,
                    _ = sleep(ACCEPT_RETRY_DELAY) => {}
                }
                continue;
            }
        };
        // 每個來源的同時連線數與新連線速率上限：超過時立即關閉，不進行 TLS 握手
        // 依序檢查：該位址的同時連線數 → 握手中的連線數上限（保留名額給已通過 hello 的連線）→
        // 該來源（IPv6 /64）的新連線速率。被前兩者拒絕的連線不扣速率額度。超過時立即關閉，不進行 TLS 握手
        let mut pre_auth = None;
        let ip_permit = match per_ip.try_acquire(peer.ip(), || {
            pre_auth = handshakes.clone().try_acquire_owned().ok();
            pre_auth.is_some()
        }) {
            Ok(p) => p,
            Err(Rejected::Concurrency) => {
                throttled!(
                    throttle,
                    (Category::PerIpLimit, peer.ip()),
                    warn,
                    "來自 {} 的同時連線數已達上限 {}（server.max_connections_per_ip），拒絕新連線",
                    peer.ip(),
                    per_ip.max_per_ip()
                );
                continue;
            }
            Err(Rejected::Gate) => {
                throttled!(
                    throttle,
                    Category::HandshakeLimit,
                    warn,
                    "握手中（尚未通過 hello）的連線數已達上限 {}，拒絕來自 {} 的新連線",
                    max_handshakes,
                    peer.ip()
                );
                continue;
            }
            Err(Rejected::Rate) => {
                throttled!(
                    throttle,
                    (Category::ConnectionRate, peer.ip()),
                    warn,
                    "來自 {} 的新連線超過每秒 {} 條（IPv6 以 /64 合併計算），拒絕新連線",
                    peer.ip(),
                    per_ip::NEW_CONNECTIONS_PER_SEC
                );
                continue;
            }
        };
        let conn_id = next_conn_id();
        debug!("[#{}] 接收到來自 {} 的連線", conn_id, peer);

        // 每條連線使用接受當下的 TLS 設定；熱重載只影響之後的新連線
        let acceptor = ctx.tls.current().acceptor.clone();
        let ctx = ctx.clone();
        let throttle = throttle.clone();
        connections.spawn(async move {
            // permit 在連線結束時釋放
            let _permit = permit;
            let _ip_permit = ip_permit;
            match handle_connection(ctx, acceptor, socket, peer, conn_id, pre_auth).await {
                Ok(()) => {}
                // 對方未送 close_notify 就斷線、連線被重設或中止：屬正常的斷線情境
                Err(e) if is_peer_disconnect(&e) => {
                    debug!("[#{}] {} 中斷連線: {}", conn_id, peer, e);
                }
                Err(e) => throttled!(
                    throttle,
                    Category::ConnectionError,
                    warn,
                    "[#{}] 處理來自 {} 的連線時發生錯誤: {}",
                    conn_id,
                    peer,
                    e
                ),
            }
        });
    }

    Ok(connections)
}

/// `run` 的結果對應的結束代碼：正常關機 0；因內部錯誤而關機或啟動失敗 1
fn exit_code(result: &Result<Option<String>, Error>) -> u8 {
    match result {
        Ok(None) => 0,
        Ok(Some(_)) | Err(_) => 1,
    }
}

fn main() -> ExitCode {
    // 日誌尚未初始化前的錯誤只能輸出到 stderr
    // args_os：非 UTF-8 的參數不會 panic，由 parse_args 回錯誤（審計 Q5）
    let config_path = match parse_args(std::env::args_os().skip(1)) {
        Ok(CliAction::Run { config_path }) => config_path,
        Ok(CliAction::Help) => {
            println!("{}", USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(CliAction::Version) => {
            println!("{}", VERSION);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("{}\n\n{}", e, USAGE);
            return ExitCode::FAILURE;
        }
    };

    let config = match AppConfig::load(config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Kairos 啟動失敗: {}", e);
            return ExitCode::FAILURE;
        }
    };

    // validate() 已檢查過 log.level，這裡仍以錯誤處理避免 panic
    let level = match config.log.level_filter() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Kairos 啟動失敗: {}", e);
            return ExitCode::FAILURE;
        }
    };
    let newest_log = match logger::setup(level) {
        Ok(newest) => newest,
        Err(e) => {
            eprintln!("Kairos 啟動失敗: {}", e);
            return ExitCode::FAILURE;
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("Kairos 啟動失敗: 無法建立 tokio runtime: {}", e);
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(
        config,
        config_path.as_deref(),
        newest_log,
        RunOptions {
            log_dir: PathBuf::from(logger::LOG_DIR),
            shutdown: Arc::new(Shutdown::new()),
            install_signal_handlers: true,
        },
    ));
    // 不無限期等待仍在執行的 blocking 工作（例如卡住的 fsync）；看門狗另外保證行程會結束
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);

    match &result {
        Ok(None) => {}
        Ok(Some(reason)) => error!("Kairos 因內部錯誤而結束: {}", reason),
        Err(e) => error!("Kairos 啟動失敗: {}", e),
    }
    log::logger().flush();
    ExitCode::from(exit_code(&result))
}

/// [`run`] 的執行環境（測試可替換）
pub(crate) struct RunOptions {
    /// 日誌目錄（日誌保留清除用）
    pub log_dir: PathBuf,
    /// 關機狀態；測試可由外部觸發
    pub shutdown: Arc<Shutdown>,
    /// 是否註冊 OS 關機訊號（第二次訊號強制結束、看門狗）
    pub install_signal_handlers: bool,
}

/// 從 `/proc/self/limits` 取得可開啟檔案數的 soft limit（`unlimited` 為 `u64::MAX`）
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_nofile_soft_limit(limits: &str) -> Option<u64> {
    let line = limits.lines().find(|l| l.starts_with("Max open files"))?;
    let soft = line
        .strip_prefix("Max open files")?
        .split_whitespace()
        .next()?;
    if soft == "unlimited" {
        Some(u64::MAX)
    } else {
        soft.parse().ok()
    }
}

/// 建議的 `LimitNOFILE`：不小於所需數量的 2 的冪次，且至少 4096
const MIN_SUGGESTED_NOFILE: u64 = 4096;

/// soft limit 低於 `max_connections` 加上預留量時的警告訊息（D4：具體指出建議做法）；足夠時回傳 `None`
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn fd_limit_warning(soft: u64, max_connections: usize) -> Option<String> {
    let needed = max_connections as u64 + FD_RESERVE;
    if soft >= needed {
        return None;
    }
    let suggested = needed
        .checked_next_power_of_two()
        .unwrap_or(needed)
        .max(MIN_SUGGESTED_NOFILE);
    let lower = if soft > FD_RESERVE {
        format!(
            "；或將 server.max_connections 調低至 {} 以下（= 目前上限 {} − 預留 {}）",
            soft - FD_RESERVE,
            soft,
            FD_RESERVE
        )
    } else {
        String::new()
    };
    Some(format!(
        "可開啟的檔案數上限（RLIMIT_NOFILE soft limit）為 {}，低於 server.max_connections（{}）加上預留的 {}（共 {}）；連線數接近上限時可能 accept 失敗（EMFILE）。建議：以 systemd 執行時在 unit 的 [Service] 加上 LimitNOFILE={}，手動執行時先執行 ulimit -n {}{}",
        soft, max_connections, FD_RESERVE, needed, suggested, suggested, lower
    ))
}

/// 可開啟檔案數的 soft limit 低於 `max_connections` 加上預留量時警告（R2-K25）。
/// 只在 Linux 讀取 `/proc/self/limits`（不需額外依賴）；其他平台不檢查。
fn warn_if_fd_limit_low(max_connections: usize) {
    #[cfg(target_os = "linux")]
    if let Ok(text) = std::fs::read_to_string("/proc/self/limits")
        && let Some(soft) = parse_nofile_soft_limit(&text)
        && let Some(message) = fd_limit_warning(soft, max_connections)
    {
        warn!("{}", message);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = max_connections;
}

/// 執行伺服器直到關機。正常關機回傳 `Ok(None)`；因背景工作異常而關機時回傳 `Ok(Some(原因))`
/// `newest_log` 為啟動前最新日誌檔的日期（日誌保留清除的日期基準）
async fn run(
    config: AppConfig,
    config_path: Option<&Path>,
    newest_log: Option<chrono::NaiveDate>,
    opts: RunOptions,
) -> Result<Option<String>, Error> {
    info!("{}", VERSION);
    let default_file = Path::new(config::DEFAULT_CONFIG_FILE);
    match config_path {
        Some(p) => info!("設定檔: {}", p.display()),
        None if default_file.is_file() => info!("設定檔: {}", default_file.display()),
        None => info!("未找到 {}，使用程式內預設值", default_file.display()),
    }
    // 預設拒絕：未啟用 mTLS 又監聽非 loopback 位址時拒絕啟動（S1）。
    // 以解析後的位址判斷（server.listen 可以是主機名稱，例如 localhost:8080）
    let resolved: Vec<std::net::SocketAddr> = tokio::net::lookup_host(&config.server.listen)
        .await
        .map_err(|e| {
            Error::new(
                e.kind(),
                format!("無法解析 server.listen {}: {}", config.server.listen, e),
            )
        })?
        .collect();
    let listen_auth = config.check_listen_auth(&resolved)?;
    warn_if_fd_limit_low(config.server.max_connections);
    if let Some((max, estimate)) = config.daily_limit_below_estimate() {
        warn!(
            "storage.max_daily_bytes（{} bytes）小於以 {} 條連線、每條每秒 1 則 report 估算的每日量（約 {} bytes）；達到上限後當天的 report 會回 storage_unavailable 而不寫入",
            max, config.server.max_connections, estimate
        );
    }

    // 載入 TLS 設定；設定 auth.client_ca 時啟用 mTLS。先取指紋再建立，之間的變更會在下一次重載偵測到。
    let tls_paths = TlsPaths {
        cert: config.server.cert_path.clone(),
        key: config.server.key_path.clone(),
        client_ca: config.auth.client_ca_path().map(str::to_string),
        crls: config.auth.crl_paths(),
    };
    let initial_fingerprint = tls::fingerprint(&tls_paths);
    let tls_state = Arc::new(TlsState::new(tls::build(&tls_paths)?));
    // 伺服器私鑰權限過寬時提醒（S8；只在 Unix 檢查，熱重載換了私鑰時再檢查一次）
    if let Some(message) = fsperm::key_permission_warning(Path::new(&tls_paths.key)) {
        warn!("{}", message);
    }
    match (&tls_paths.client_ca, &listen_auth) {
        (Some(ca), _) => {
            info!(
                "已啟用客戶端憑證驗證（mTLS），信任 {}，CRL {} 份；agent_id 必須與憑證的 SAN 相符",
                ca,
                tls_state.current().crl_count
            );
            if config.auth.allow_unauthenticated {
                info!("已啟用 mTLS，auth.allow_unauthenticated 不生效");
            }
        }
        (None, ListenAuth::UnauthenticatedAllowed { exposed }) => warn!(
            "未啟用客戶端憑證驗證（auth.client_ca）卻監聽非 loopback 位址 {}（auth.allow_unauthenticated = true）：任何能連線的人都能以任意 agent_id 註冊、寫入 report，或把合法的 agent 踢下線。僅限隔離的測試環境，正式環境請啟用 mTLS",
            exposed
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("、")
        ),
        (None, _) => warn!(
            "未啟用客戶端憑證驗證（auth.client_ca），只監聽 loopback 位址；同一台主機上的任何程式都能以任意 agent_id 註冊，僅建議用於本機開發"
        ),
    }

    // 準備資料目錄與 report 寫入工作（按 UTC 日期分檔，啟動時與每小時清除過期檔案）
    let paths = storage::prepare_data_dir(Path::new(&config.storage.data_dir)).await?;
    // 既有的資料、日誌目錄對其他使用者開放時提醒（S3；Kairos 不自行改變既有目錄的權限）
    for (dir, what) in [
        (paths.data_dir.as_path(), "資料目錄"),
        (opts.log_dir.as_path(), "日誌目錄"),
    ] {
        if let Some(message) = fsperm::dir_permission_warning(dir, what) {
            warn!("{}", message);
        }
    }
    let (reports, writer_task) = storage::start_report_writer(
        paths.data_dir.clone(),
        config.storage.retention_days,
        config.storage.max_daily_bytes,
        chrono::Utc::now,
        storage::RETENTION_SWEEP_INTERVAL,
    )
    .await?;

    let listener = TcpListener::bind(&config.server.listen)
        .await
        .map_err(|e| {
            Error::new(
                e.kind(),
                format!("無法監聽 {}: {}", config.server.listen, e),
            )
        })?;
    let local_addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| config.server.listen.clone());

    // 關機訊號：第一次記下 T0 並取消，第二次直接結束行程
    let shutdown = opts.shutdown.clone();
    if opts.install_signal_handlers {
        tokio::spawn(shutdown::watch(shutdown::Signals::new()?, shutdown.clone()));
    }

    let registry = Arc::new(Registry::with_max_agents(config.max_agents()));
    let writer = supervise(
        "report 寫入工作",
        tokio::spawn(writer_task),
        shutdown.clone(),
    );
    // monitor 要等所有連線結束後才停止，因此使用獨立的 token
    let monitor_stop = CancellationToken::new();
    let monitor = supervise(
        "監控工作",
        tokio::spawn(monitor::run(
            registry.clone(),
            paths.clone(),
            monitor_stop.clone(),
        )),
        shutdown.clone(),
    );
    supervise(
        "日誌清除工作",
        tokio::spawn(logger::run_cleanup(
            opts.log_dir.clone(),
            config.log.retention_days,
            newest_log,
            shutdown.clone(),
        )),
        shutdown.clone(),
    );

    // TLS 熱重載與現役連線的客戶端憑證重新驗證
    match config.server.reload_interval_secs {
        0 => warn!(
            "已停用 TLS 熱重載（server.reload_interval_secs = 0）：更換憑證、CA 或 CRL 需重新啟動，現役連線的客戶端憑證也不會重新驗證"
        ),
        secs => {
            info!("每 {} 秒檢查憑證、私鑰、客戶端 CA 與 CRL 是否變更", secs);
            supervise(
                "TLS 重載工作",
                tokio::spawn(reload::run(
                    reload::Reloader::new(
                        tls_paths.clone(),
                        initial_fingerprint,
                        tls_state.clone(),
                    ),
                    registry.clone(),
                    tls_state.clone(),
                    Duration::from_secs(secs),
                    shutdown.clone(),
                )),
                shutdown.clone(),
            );
        }
    }

    let throttle = Arc::new(LogThrottle::default());
    let ctx = Arc::new(ConnContext {
        registry: registry.clone(),
        reports,
        timeouts: config.timeouts.clone(),
        shutdown: shutdown.clone(),
        tls: tls_state.clone(),
        throttle: throttle.clone(),
        binding: BindingPolicy {
            allow_multiple_dns_sans: config.auth.allow_multiple_dns_sans,
        },
        report_limiter: Arc::new(ReportLimiter::new(config.max_connections_per_ip())),
    });
    let per_ip = PerIpLimiter::new(config.max_connections_per_ip());

    info!(
        "Kairos 已啟動，監聽 {}（連線上限 {}，其中握手中 {}，每個來源 IP {}，狀態表上限 {}，資料目錄 {}）",
        local_addr,
        config.server.max_connections,
        handshake_limit(config.server.max_connections),
        config.max_connections_per_ip(),
        config.max_agents(),
        config.storage.data_dir
    );

    let mut connections =
        accept_loop(listener, ctx.clone(), config.server.max_connections, per_ip).await?;

    // ---- 關機 ----
    // 所有階段共用以 T0（收到訊號的時間）起算的期限
    let t0 = shutdown.started_at().unwrap_or_else(Instant::now);
    info!("已停止接受新連線");

    while connections.try_join_next().is_some() {}
    info!(
        "關閉 {} 條連線（最晚至訊號後 {:.1} 秒）",
        connections.len(),
        CONNECTIONS_DEADLINE.as_secs_f64()
    );
    let drained = timeout_at(tokio_deadline(t0 + CONNECTIONS_DEADLINE), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        warn!("{} 條連線未在期限內結束，強制中止", connections.len());
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
    // 被強制中止的連線來不及自行標記離線
    let forced = registry.mark_all_offline(Instant::now());
    if forced > 0 {
        info!("將 {} 個仍在線上的 agent 標記為離線", forced);
    }

    // 釋放最後一個 report sender，writer 取完佇列、寫入並逐筆回覆後結束
    let pending = ctx.reports.pending();
    drop(ctx);
    info!("寫入剩餘 {} 筆 report", pending);
    match timeout_at(tokio_deadline(t0 + WRITER_DEADLINE), writer).await {
        // 異常結束已由 supervise 記錄
        Ok(_) => {}
        Err(_) => error!(
            "report 寫入未在訊號後 {:.1} 秒內完成，佇列中的資料可能遺失",
            WRITER_DEADLINE.as_secs_f64()
        ),
    }

    // 停止 monitor，再寫一次 agents.json。最後的快照由這裡寫出，monitor 異常結束時也不會漏掉（R2-K17）
    monitor_stop.cancel();
    let deadline = tokio_deadline(t0 + MONITOR_DEADLINE);
    let _ = timeout_at(deadline, monitor).await;
    if timeout_at(deadline, monitor::write_final_snapshot(&registry, &paths))
        .await
        .is_err()
    {
        error!(
            "最後的 agent 快照未在訊號後 {:.1} 秒內寫完",
            MONITOR_DEADLINE.as_secs_f64()
        );
    }

    info!(
        "Kairos 已結束（關機耗時 {:.2} 秒）",
        t0.elapsed().as_secs_f64()
    );
    Ok(shutdown.fatal_reason().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturation_logs_only_on_transitions() {
        let mut s = Saturation::new(100);
        // 未飽和時不回報解除
        assert!(!s.check_resume(100));
        // 進入飽和只回報一次
        assert!(s.enter());
        assert!(!s.enter());
        assert!(!s.enter());
        // 名額未回到 10% 前不算解除
        assert!(!s.check_resume(0));
        assert!(!s.check_resume(9));
        assert!(s.check_resume(10));
        assert!(!s.check_resume(50));
        // 再次飽和會再回報
        assert!(s.enter());
    }

    /// 測試用的 fatal 處理：只觸發關機，不啟動看門狗
    fn fatal_without_watchdog(s: &Shutdown, reason: String) {
        s.trigger_fatal(reason);
    }

    #[tokio::test]
    async fn supervise_triggers_shutdown_when_task_ends_early() {
        // K-C9：背景工作在關機前 panic，立即觸發關機並記下原因
        let s = Arc::new(Shutdown::new());
        let panicking = tokio::spawn(async { panic!("boom") });
        supervise_with("測試工作", panicking, s.clone(), fatal_without_watchdog)
            .await
            .unwrap();
        assert!(s.started_at().is_some());
        assert!(s.fatal_reason().unwrap().contains("測試工作異常結束"));

        // 在關機前正常返回也是異常
        let s = Arc::new(Shutdown::new());
        supervise_with(
            "測試工作",
            tokio::spawn(async {}),
            s.clone(),
            fatal_without_watchdog,
        )
        .await
        .unwrap();
        assert!(s.fatal_reason().unwrap().contains("測試工作意外結束"));

        // 關機期間結束屬正常（panic 只記錄），不設定 fatal 原因
        let s = Arc::new(Shutdown::new());
        s.trigger();
        supervise_with(
            "測試工作",
            tokio::spawn(async {}),
            s.clone(),
            fatal_without_watchdog,
        )
        .await
        .unwrap();
        let panicking = tokio::spawn(async { panic!("boom") });
        supervise_with("測試工作", panicking, s.clone(), fatal_without_watchdog)
            .await
            .unwrap();
        assert_eq!(s.fatal_reason(), None);
    }

    #[test]
    fn handshake_limit_reserves_half_for_registered_connections() {
        assert_eq!(handshake_limit(1024), 512);
        assert_eq!(handshake_limit(3), 1);
        assert_eq!(handshake_limit(1), 1);
    }

    #[test]
    fn accept_errors_are_classified() {
        // R2-K14：只影響單一連線的錯誤不等待；資源問題才等待重試
        for kind in [
            ErrorKind::ConnectionAborted,
            ErrorKind::ConnectionReset,
            ErrorKind::Interrupted,
        ] {
            assert!(accept_error_is_per_connection(&Error::new(kind, "x")));
        }
        assert!(!accept_error_is_per_connection(&Error::other("EMFILE")));
        assert!(!accept_error_is_per_connection(&Error::new(
            ErrorKind::OutOfMemory,
            "x"
        )));
    }

    #[test]
    fn nofile_soft_limit_is_parsed_from_proc_limits() {
        let text = "Limit                     Soft Limit           Hard Limit           Units     \n\
Max cpu time              unlimited            unlimited            seconds   \n\
Max open files            1024                 524288               files     \n";
        assert_eq!(parse_nofile_soft_limit(text), Some(1024));
        let text = "Max open files            unlimited            unlimited            files\n";
        assert_eq!(parse_nofile_soft_limit(text), Some(u64::MAX));
        assert_eq!(parse_nofile_soft_limit("nothing"), None);
    }

    #[test]
    fn fd_limit_warning_suggests_concrete_fixes() {
        // D4：Ubuntu 預設 soft 1024、範例設定 max_connections = 1024
        let message = fd_limit_warning(1024, 1024).unwrap();
        assert_eq!(
            message,
            "可開啟的檔案數上限（RLIMIT_NOFILE soft limit）為 1024，低於 server.max_connections（1024）加上預留的 64（共 1088）；連線數接近上限時可能 accept 失敗（EMFILE）。建議：以 systemd 執行時在 unit 的 [Service] 加上 LimitNOFILE=4096，手動執行時先執行 ulimit -n 4096；或將 server.max_connections 調低至 960 以下（= 目前上限 1024 − 預留 64）"
        );
        // 足夠時不警告
        assert_eq!(fd_limit_warning(1088, 1024), None);
        assert_eq!(fd_limit_warning(u64::MAX, 65535), None);
        // 需要量較大時建議值隨之提高；soft 小於預留量時不建議調低 max_connections
        let message = fd_limit_warning(32, 10_000).unwrap();
        assert!(message.contains("LimitNOFILE=16384"), "{message}");
        assert!(!message.contains("調低"), "{message}");
    }

    #[test]
    fn version_string_has_package_version_and_commit() {
        // D3：`kairos <版本> (<commit>)`
        let expected_prefix = format!("kairos {} (", env!("CARGO_PKG_VERSION"));
        assert!(VERSION.starts_with(&expected_prefix), "{VERSION}");
        assert!(VERSION.ends_with(')'), "{VERSION}");
        let commit = &VERSION[expected_prefix.len()..VERSION.len() - 1];
        assert!(
            !commit.is_empty() && !commit.contains(char::is_whitespace),
            "{VERSION}"
        );
    }

    fn run_config(dir: &Path) -> AppConfig {
        let pki = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/pki/");
        let mut c = AppConfig::default();
        c.server.listen = "127.0.0.1:0".into();
        c.server.cert_path = format!("{pki}server.crt");
        c.server.key_path = format!("{pki}server.key");
        c.server.reload_interval_secs = 1;
        c.storage.data_dir = dir.join("data").to_string_lossy().into_owned();
        c
    }

    fn run_options(dir: &Path) -> (RunOptions, Arc<Shutdown>) {
        let shutdown = Arc::new(Shutdown::new());
        (
            RunOptions {
                log_dir: dir.join("logs"),
                shutdown: shutdown.clone(),
                install_signal_handlers: false,
            },
            shutdown,
        )
    }

    #[tokio::test]
    async fn run_shuts_down_within_the_deadline_and_reports_exit_code() {
        // R2-T1：main::run 收到關機後在期限內結束、寫出最後的 agents.json，結束代碼 0；
        // 因內部錯誤而關機時回傳原因，結束代碼 1
        let dir = std::env::temp_dir().join(format!(
            "kairos-run-{}-{}",
            std::process::id(),
            next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let (opts, shutdown) = run_options(&dir);
        let task = tokio::spawn(run(run_config(&dir), None, None, opts));
        // 等到啟動完成（資料目錄與日誌清除的狀態檔都已建立）
        for _ in 0..250 {
            if dir.join("data").join(storage::AGENTS_FILE).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        std::fs::remove_file(dir.join("data").join(storage::AGENTS_FILE)).unwrap();
        let t0 = shutdown.trigger();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("run 應在關機後結束")
            .unwrap();
        assert!(
            t0.elapsed() < shutdown::WATCHDOG_DEADLINE,
            "{:?}",
            t0.elapsed()
        );
        assert!(matches!(result, Ok(None)), "{result:?}");
        assert_eq!(exit_code(&result), 0);
        // 最後的快照由 run 寫出
        assert!(dir.join("data").join(storage::AGENTS_FILE).exists());

        // 內部錯誤：結束代碼 1
        let (opts, shutdown) = run_options(&dir);
        let task = tokio::spawn(run(run_config(&dir), None, None, opts));
        tokio::time::sleep(Duration::from_millis(200)).await;
        shutdown.trigger_fatal("測試用的內部錯誤");
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            result.as_ref().unwrap().as_deref(),
            Some("測試用的內部錯誤")
        );
        assert_eq!(exit_code(&result), 1);

        // 啟動失敗（憑證不存在）：結束代碼 1
        let (opts, _) = run_options(&dir);
        let mut bad = run_config(&dir);
        bad.server.cert_path = dir.join("missing.crt").to_string_lossy().into_owned();
        let result = run(bad, None, None, opts).await;
        assert!(result.is_err());
        assert_eq!(exit_code(&result), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_refuses_unauthenticated_non_loopback_listen() {
        // S1：未啟用 mTLS 又監聽非 loopback 位址時拒絕啟動，且在建立資料目錄與監聽之前
        let dir = std::env::temp_dir().join(format!(
            "kairos-run-s1-{}-{}",
            std::process::id(),
            next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = run_config(&dir);
        config.server.listen = "0.0.0.0:0".into();
        let (opts, _) = run_options(&dir);
        let err = run(config.clone(), None, None, opts).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
        assert!(
            err.to_string().contains("auth.allow_unauthenticated"),
            "{err}"
        );
        assert!(!dir.join("data").exists(), "拒絕啟動前不應建立資料目錄");

        // 明確允許時照常啟動
        config.auth.allow_unauthenticated = true;
        let (opts, shutdown) = run_options(&dir);
        let task = tokio::spawn(run(config, None, None, opts));
        for _ in 0..250 {
            if dir.join("data").join(storage::AGENTS_FILE).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        shutdown.trigger();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("run 應在關機後結束")
            .unwrap();
        assert!(matches!(result, Ok(None)), "{result:?}");

        // 主機名稱解析成 loopback 時照常啟動（不需 allow_unauthenticated）
        let mut config = run_config(&dir);
        config.server.listen = "localhost:0".into();
        let (opts, shutdown) = run_options(&dir);
        let task = tokio::spawn(run(config, None, None, opts));
        tokio::time::sleep(Duration::from_millis(200)).await;
        shutdown.trigger();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Ok(None)), "{result:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saturation_threshold_at_least_one() {
        let mut s = Saturation::new(1);
        assert!(s.enter());
        assert!(!s.check_resume(0));
        assert!(s.check_resume(1));
    }
}
