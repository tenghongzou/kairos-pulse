mod alerts;
mod alerts_cli;
mod api;
mod audit;
mod audit_cli;
mod audit_monitor;
mod auth;
mod bus;
mod config;
mod connection;
mod dedup;
mod diag;
mod enroll;
mod enroll_cli;
mod expiry;
mod export_cli;
mod fsperm;
mod hosts_cli;
mod import_cli;
#[cfg(test)]
mod integration_tests;
mod keys_backup;
mod logger;
mod metrics;
mod monitor;
mod per_ip;
mod pki;
mod pki_cli;
mod privacy;
mod privacy_cli;
mod probe;
mod protocol;
mod rate;
mod rbac;
mod registry;
mod reload;
mod retention;
mod settings;
mod shutdown;
mod stats;
mod storage;
mod store;
#[cfg(test)]
mod test_pki;
mod testhooks;
mod throttle;
mod tls;
mod token_cli;
mod users;
mod users_cli;

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
use crate::config::{
    AppConfig, CliAction, FD_RESERVE, ListenAuth, StorageBackend, USAGE, parse_args,
    suggested_nofile,
};
use crate::connection::{ConnContext, handle_connection, is_peer_disconnect};
use crate::per_ip::{ConnectionSlot, PerIpLimiter, PerIpPolicy, Rejected};
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

/// 告警引擎的時鐘（Unix 毫秒；測試以假的時鐘驅動引擎本身）
fn alerts_clock() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// accept 因資源問題失敗（例如檔案描述符用盡）後的等待時間
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// 等待新連線期間檢查「是否已解除飽和」的間隔
const SATURATION_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// `block_on` 返回後，等待 blocking 執行緒（例如卡住的 fsync）結束的上限；
/// 超過就不再等待，讓行程以 `run` 的結果結束（R2-K17）
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);

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
/// 被拒的連線與全域上限的暫停時間記錄在 `per_ip` 的計數器（[`PerIpLimiter::stats`]）。
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
                per_ip.stats().pause_begin(Instant::now());
                if saturation.enter() {
                    warn!(
                        "已達同時連線上限 {}（server.max_connections），暫停接受新連線；之後的新連線在 TCP 層排隊或逾時",
                        max_connections
                    );
                }
                let permit = tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break 'accept,
                    res = limit.clone().acquire_owned() => {
                        res.map_err(|_| Error::other("連線上限的 Semaphore 已關閉"))?
                    }
                };
                per_ip.stats().pause_end(Instant::now());
                permit
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
                        let global = per_ip.stats().snapshot(Instant::now()).global;
                        info!(
                            "連線數已降至上限 {} 以下，恢復接受新連線（啟動以來因全域上限暫停 {} 次、累計 {:.1} 秒）",
                            max_connections,
                            global.pauses,
                            global.paused.as_secs_f64()
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
        // 依序檢查：該位址的連線數（mTLS 時只計算握手中的連線）→ 握手中的連線數上限（保留名額給已通過
        // hello 的連線）→ 該來源（IPv6 /64）的新連線速率。被前兩者拒絕的連線不扣速率額度。
        // 超過任一上限時立即關閉，不進行 TLS 握手；被拒的連線依原因計數（per_ip::stats）
        let mut pre_auth = None;
        let ip_permit = match per_ip.try_acquire(peer.ip(), || {
            pre_auth = handshakes.clone().try_acquire_owned().ok();
            pre_auth.is_some()
        }) {
            Ok(p) => p,
            Err(Rejected::Concurrency(limit)) => {
                // 設計包 KRS-W2201 的情境：寫出計算方式與要調的設定
                let (counted, setting, hint) = match (per_ip.policy().mtls, limit.allowlisted) {
                    (false, false) => (
                        "同時連線數",
                        "server.max_connections_per_ip",
                        "；若這是多台主機共用的 NAT 出口，可將它加入 server.per_ip_allowlist",
                    ),
                    (true, false) => (
                        "握手中（尚未通過 hello）的連線數",
                        "server.max_connections_per_ip；啟用 mTLS 時通過 hello 的連線改依憑證身分計數",
                        "；若這是多台主機共用的 NAT 出口，可將它加入 server.per_ip_allowlist",
                    ),
                    (false, true) => (
                        "同時連線數",
                        "此位址在 server.per_ip_allowlist 中，上限為 server.max_connections_per_allowlisted_ip 與 server.max_connections 的較小者",
                        "",
                    ),
                    (true, true) => (
                        "握手中（尚未通過 hello）的連線數",
                        "此位址在 server.per_ip_allowlist 中，上限為 server.max_connections_per_allowlisted_ip 與 server.max_connections 的較小者",
                        "",
                    ),
                };
                throttled!(
                    throttle,
                    (Category::PerIpLimit, peer.ip()),
                    warn,
                    "來自 {} 的{}已達上限 {}（{}），在 TLS 握手前拒絕新連線{}",
                    peer.ip(),
                    counted,
                    limit.max,
                    setting,
                    hint
                );
                continue;
            }
            Err(Rejected::Gate) => {
                throttled!(
                    throttle,
                    Category::HandshakeLimit,
                    warn,
                    "握手中（尚未通過 hello）的連線數已達上限 {}（server.max_connections 的一半），在 TLS 握手前拒絕來自 {} 的新連線",
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
        // 來源位址與握手中的名額：hello 通過後依兩階段釋放（per_ip::ConnectionSlot）
        let slot = ConnectionSlot::new(ip_permit, pre_auth);
        connections.spawn(async move {
            // 全域名額在連線結束時釋放
            let _permit = permit;
            match handle_connection(ctx, acceptor, socket, peer, conn_id, slot).await {
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

    // 兩類被拒連線的累計值（S20／`/metrics` 尚未實作前，至少在關機時留下紀錄）
    let summary = per_ip.snapshot(Instant::now()).summary();
    info!("{}", summary);
    Ok(connections)
}

// ---- 結束代碼 ----
//
// 依設計包 `design/design-system/cli-style.md` §8（`kairos` 與 `pulse` 共用的結束代碼表）。
// 常駐模式只用到下列代碼，另有正常結束 0、第二次收到關機訊號時的 130（`shutdown::FORCED_EXIT_CODE`）。
// 不使用 sysexits（64、78 等）。`packaging/kairos.service` 以 `RestartPreventExitStatus` 讓 systemd
// 不重試「重試也不會成功」的代碼（2、3、4）。

/// 一般錯誤：內部錯誤（背景工作異常、看門狗強制結束）與未分類的啟動失敗（例如資料目錄、
/// 日誌目錄無法建立），交由服務管理員重新啟動
const EXIT_GENERAL: u8 = 1;
/// 用法錯誤：命令列參數無法辨識、`--config` 缺少路徑或重複指定
const EXIT_USAGE: u8 = 2;
/// 設定錯誤：設定檔無法載入或驗證不通過、`auth.allow_unauthenticated` 未設定卻未啟用 mTLS
/// 監聽對外位址（S1）等
const EXIT_CONFIG: u8 = 3;
/// 憑證錯誤：伺服器憑證、私鑰、客戶端 CA 或 CRL 無法讀取、格式錯誤或不成對
const EXIT_CERT: u8 = 4;
/// 衝突：`server.listen` 的連接埠已被佔用（例如另一個 Kairos 仍在執行）。
/// 舊的行程結束後重試可能成功，因此不列入 `RestartPreventExitStatus`
const EXIT_CONFLICT: u8 = 11;

/// 帶有結束代碼的 [`Error`] 內容；訊息與 `kind` 不變
#[derive(Debug)]
struct Classified {
    code: u8,
    inner: Error,
}

impl std::fmt::Display for Classified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(f)
    }
}

impl std::error::Error for Classified {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner.source()
    }
}

/// 為錯誤標上結束代碼（保留原本的 `kind` 與訊息）
fn classify(e: Error, code: u8) -> Error {
    Error::new(e.kind(), Classified { code, inner: e })
}

/// 標記為設定錯誤（[`EXIT_CONFIG`]）
fn config_error(e: Error) -> Error {
    classify(e, EXIT_CONFIG)
}

/// 標記為憑證錯誤（[`EXIT_CERT`]）
fn cert_error(e: Error) -> Error {
    classify(e, EXIT_CERT)
}

/// 監聽失敗：連接埠被佔用時標記為衝突（[`EXIT_CONFLICT`]），其他原因不標記（[`EXIT_GENERAL`]）
fn listen_error(e: Error) -> Error {
    if e.kind() == ErrorKind::AddrInUse {
        classify(e, EXIT_CONFLICT)
    } else {
        e
    }
}

/// 錯誤上標記的結束代碼（未標記時為 `None`）
fn classified_code(e: &Error) -> Option<u8> {
    e.get_ref()
        .and_then(|inner| inner.downcast_ref::<Classified>())
        .map(|c| c.code)
}

/// `run` 的結果對應的結束代碼：正常關機 0；標記過的啟動失敗為其代碼（3、4、11）；
/// 因內部錯誤而關機或其他啟動失敗 [`EXIT_GENERAL`]（交由服務管理員重新啟動）
fn exit_code(result: &Result<Option<String>, Error>) -> u8 {
    match result {
        Ok(None) => 0,
        Err(e) => classified_code(e).unwrap_or(EXIT_GENERAL),
        Ok(Some(_)) => EXIT_GENERAL,
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
        // 子命令：查詢後結束，不啟動伺服器、不寫日誌檔
        Ok(CliAction::Hosts(cmd)) => return ExitCode::from(hosts_cli::run(cmd)),
        Ok(CliAction::Reports(cmd)) => return ExitCode::from(export_cli::run(cmd)),
        Ok(CliAction::Db(cmd)) => return ExitCode::from(export_cli::run_db(cmd)),
        Ok(CliAction::ApiToken(cmd)) => return ExitCode::from(token_cli::run(cmd)),
        Ok(CliAction::Enroll(cmd)) => return ExitCode::from(enroll_cli::run(cmd)),
        Ok(CliAction::Alerts(cmd)) => return ExitCode::from(alerts_cli::run(cmd)),
        Ok(CliAction::Users(cmd)) => return ExitCode::from(users_cli::run(cmd)),
        Ok(CliAction::Audit(cmd)) => return ExitCode::from(audit_cli::run(cmd)),
        Ok(CliAction::Diag(args)) => return ExitCode::from(diag::run(args)),
        Ok(CliAction::CheckConfig(args)) => return ExitCode::from(diag::run_check(args)),
        Err(e) => {
            let usage = match std::env::args_os().nth(1) {
                Some(a) if a == "hosts" => hosts_cli::HOSTS_USAGE,
                Some(a) if a == "api-token" => token_cli::TOKEN_USAGE,
                Some(a) if a == "reports" => {
                    if std::env::args_os().nth(2).is_some_and(|a| a == "import") {
                        import_cli::IMPORT_USAGE
                    } else {
                        export_cli::REPORTS_USAGE
                    }
                }
                Some(a) if a == "db" => export_cli::DB_USAGE,
                Some(a) if a == "enroll-token" => enroll_cli::ENROLL_TOKEN_USAGE,
                Some(a) if a == "enroll" => enroll_cli::ENROLL_USAGE,
                Some(a) if a == "pki" => enroll_cli::PKI_USAGE,
                Some(a) if a == "config" => enroll_cli::CONFIG_USAGE,
                Some(a) if a == "alerts" => alerts_cli::ALERTS_USAGE,
                Some(a) if a == "silences" => alerts_cli::SILENCES_USAGE,
                Some(a) if a == "alert-rules" => alerts_cli::RULES_USAGE,
                Some(a) if a == "alert-channels" => alerts_cli::CHANNELS_USAGE,
                Some(a) if a == "users" => users_cli::USERS_USAGE,
                Some(a) if a == "audit" => audit_cli::AUDIT_USAGE,
                Some(a) if a == "diag" => diag::DIAG_USAGE,
                Some(a) if a == "check-config" => diag::CHECK_USAGE,
                _ if std::env::args_os().any(|a| a == "--check-config") => diag::CHECK_USAGE,
                _ => USAGE,
            };
            eprintln!("{}\n\n{}", e, usage);
            return ExitCode::from(EXIT_USAGE);
        }
    };

    // 設定錯誤以 EXIT_CONFIG 結束，systemd 不再重試（RestartPreventExitStatus）
    // 訊息帶產品代碼（KRS-E0101／E0401／E0402／E0201／E0202，與 --check-config 相同的判斷；
    // 設計包 decisions/INTEGRATION.md §28.5），結束代碼不變
    let config = match AppConfig::load(config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            let code = diag::config_load_code(&e, config_path.as_deref());
            eprintln!("錯誤 {code}：Kairos 啟動失敗: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    // validate() 已檢查過 log.level，這裡仍以錯誤處理避免 panic
    let level = match config.log.level_filter() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("錯誤 {}：Kairos 啟動失敗: {e}", diag::CODE_CONFIG_RANGE);
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    let newest_log = match logger::setup(level, &config.log) {
        Ok(newest) => newest,
        Err(e) => {
            eprintln!("Kairos 啟動失敗: {}", e);
            return ExitCode::from(EXIT_GENERAL);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("Kairos 啟動失敗: 無法建立 tokio runtime: {}", e);
            return ExitCode::from(EXIT_GENERAL);
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
    logger::flush_all();
    ExitCode::from(exit_code(&result))
}

/// 系統事件（啟動、關機）寫入稽核雜湊鏈（SQLite 時；失敗只記日誌）
async fn system_audit(
    svc: Option<&Arc<enroll::EnrollService>>,
    action: &'static str,
    config_path: Option<&Path>,
) {
    let Some(svc) = svc else {
        return;
    };
    let details = serde_json::json!({
        "version": VERSION,
        "config_file": config_path.map(|p| p.display().to_string()),
    });
    let now = chrono::Utc::now().timestamp_millis();
    let r = svc
        .db
        .write(move |conn| {
            crate::audit::append(
                conn,
                now,
                &crate::audit::Audit {
                    actor: "system",
                    category: "system",
                    action,
                    result: "ok",
                    details: Some(details),
                    ..Default::default()
                },
            )?;
            Ok(())
        })
        .await;
    if let Err(e) = r {
        warn!("無法寫入稽核紀錄（{action}）：{e}");
    }
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

/// soft limit 低於需要量（`server.max_connections` ＋ 主控台的連線上限 ＋ 預留，設計包 §24 #18、D21）
/// 時的警告訊息（D4：具體指出建議做法）；足夠時回傳 `None`
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn fd_limit_warning(
    soft: u64,
    max_connections: usize,
    console_connections: u64,
    enroll_connections: u64,
) -> Option<String> {
    let needed = max_connections as u64 + console_connections + enroll_connections + FD_RESERVE;
    if soft >= needed {
        return None;
    }
    let suggested = suggested_nofile(needed);
    let reserve = console_connections + enroll_connections + FD_RESERVE;
    let lower = if soft > reserve {
        format!(
            "；或將 server.max_connections 調低至 {} 以下（= 目前上限 {} − 預留 {}）",
            soft - reserve,
            soft,
            reserve
        )
    } else {
        String::new()
    };
    let mut console = if console_connections > 0 {
        format!("、主控台的連線上限（console.max_connections {console_connections}）")
    } else {
        String::new()
    };
    if enroll_connections > 0 {
        console.push_str(&format!(
            "、註冊端點的連線上限（enroll.max_connections {enroll_connections}）"
        ));
    }
    Some(format!(
        "可開啟的檔案數上限（RLIMIT_NOFILE soft limit）為 {}，低於 server.max_connections（{}）{}加上預留的 {}（共 {}）；連線數接近上限時可能 accept 失敗（EMFILE）。建議：以 systemd 執行時在 unit 的 [Service] 加上 LimitNOFILE={}，手動執行時先執行 ulimit -n {}{}",
        soft, max_connections, console, FD_RESERVE, needed, suggested, suggested, lower
    ))
}

/// 可開啟檔案數的 soft limit（`unlimited` 為 `u64::MAX`）；只在 Linux 讀取 `/proc/self/limits`，
/// 其他平台為 `None`（系統健康與啟動檢查共用）
pub(crate) fn nofile_soft_limit() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        parse_nofile_soft_limit(&std::fs::read_to_string("/proc/self/limits").ok()?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// 可開啟檔案數的 soft limit 低於需要量時警告（R2-K25；需要量含主控台，D21）
fn warn_if_fd_limit_low(config: &AppConfig) {
    let console = if config.console.enabled {
        config.console.max_connections as u64
    } else {
        0
    };
    let enroll = config.extra_connections() - console;
    if let Some(soft) = nofile_soft_limit()
        && let Some(message) =
            fd_limit_warning(soft, config.server.max_connections, console, enroll)
    {
        warn!("{}", message);
    }
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
    let listen_auth = config.check_listen_auth(&resolved).map_err(config_error)?;
    warn_if_fd_limit_low(&config);
    if let Some((max, estimate)) = config.total_limit_below_estimate() {
        warn!(
            "storage.max_total_bytes（{} bytes）小於以 {} 台主機、每 5 秒一筆 report 與目前保留天數估算的容量（約 {} bytes，見 STORAGE.md「容量估算」）；達到上限後會提早刪除最舊的原始 report 與彙總",
            max, config.server.max_connections, estimate
        );
    }
    if config.storage.writes_jsonl()
        && let Some((max, estimate)) = config.daily_limit_below_estimate()
    {
        warn!(
            "storage.max_daily_bytes（{} bytes）小於以 {} 條連線、每條每秒 1 則 report 估算的每日量（約 {} bytes）；達到上限後當天的 report 會回 storage_unavailable 而不寫入",
            max, config.server.max_connections, estimate
        );
    }

    // 載入 TLS 設定；設定 auth.client_ca 時啟用 mTLS。先取指紋再建立，之間的變更會在下一次重載偵測到。
    // Kairos 簽發 CA 的信任錨與 CRL（kairos/PKI.md 第 2、10 節）：SQLite 且啟用 mTLS 時才可能存在
    let pki_files = (config.storage.backend == StorageBackend::Sqlite
        && config.auth.client_ca_path().is_some())
    .then(|| {
        let data_dir = Path::new(&config.storage.data_dir);
        (
            pki::issuing_cas_path(data_dir)
                .to_string_lossy()
                .into_owned(),
            pki::issuing_crl_path(data_dir)
                .to_string_lossy()
                .into_owned(),
            pki::retired_cas_path(data_dir)
                .to_string_lossy()
                .into_owned(),
        )
    });
    let tls_paths = TlsPaths {
        cert: config.server.cert_path.clone(),
        key: config.server.key_path.clone(),
        client_ca: config.auth.client_ca_path().map(str::to_string),
        crls: config.auth.crl_paths(),
        issuing_cas: pki_files.as_ref().map(|(a, _, _)| a.clone()),
        issuing_crl: pki_files.as_ref().map(|(_, c, _)| c.clone()),
        retired_cas: pki_files.map(|(_, _, r)| r),
    };
    let initial_fingerprint = tls::fingerprint(&tls_paths);
    let tls_state = Arc::new(TlsState::new(tls::build(&tls_paths).map_err(cert_error)?));
    // 伺服器憑證與 CRL 的到期預警（O1）：啟動時檢查一次，之後由重載工作（或停用重載時的定期工作）檢查
    let mut expiry_warner = expiry::ExpiryWarner::new(config.expiry_policy());
    expiry_warner.check_and_log(&tls_state.current().expiries, true);
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

    // 準備資料目錄與 report 的儲存（SQLite 或 JSONL，見 store 模組）
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
    let store::Storage {
        reports,
        writer: writer_task,
        inventory,
        restored,
        assets,
        purger,
    } = store::start(&config, &paths.data_dir, chrono::Utc::now).await?;
    // 個資政策（[privacy]，kairos/PRIVACY.md）：要求假名化時金鑰檔一定要可用，否則不啟動
    let privacy = Arc::new(
        privacy::PrivacyPolicy::for_service(config.privacy, &paths.data_dir).map_err(|e| {
            error!(
                event = "privacy.key_unavailable", code = privacy::CODE_KEY_UNAVAILABLE;
                "錯誤 {}：{e}",
                privacy::CODE_KEY_UNAVAILABLE
            );
            e
        })?,
    );
    if let Some(d) = config.privacy.describe() {
        info!("個資保護（[privacy]）：{d}");
    }

    let listener = TcpListener::bind(&config.server.listen)
        .await
        .map_err(|e| {
            listen_error(Error::new(
                e.kind(),
                format!("無法監聽 {}: {}", config.server.listen, e),
            ))
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
    // 主機清冊中最近 24 小時出現過的主機以離線狀態放回狀態表（A6）
    if !restored.is_empty() {
        let n = registry.restore(restored, Instant::now(), chrono::Utc::now());
        info!(
            "已從主機清冊載回 {} 台最近 24 小時內出現過的主機（離線）",
            n
        );
    }
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
            inventory.clone(),
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
        0 => {
            warn!(
                "已停用 TLS 熱重載（server.reload_interval_secs = 0）：更換憑證、CA 或 CRL 需重新啟動，現役連線的客戶端憑證也不會重新驗證"
            );
            // Kairos 自己產生的 CRL 與簽發 CA 的信任錨仍要立即生效
            supervise(
                "TLS 重建工作",
                tokio::spawn(reload::run_on_trigger(
                    reload::Reloader::new(
                        tls_paths.clone(),
                        initial_fingerprint,
                        tls_state.clone(),
                    ),
                    registry.clone(),
                    tls_state.clone(),
                    shutdown.clone(),
                )),
                shutdown.clone(),
            );
            // 到期預警仍定期檢查（O1）
            supervise(
                "到期預警工作",
                tokio::spawn(expiry::run(
                    expiry_warner,
                    tls_state.clone(),
                    expiry::PERIODIC_CHECK_WITHOUT_RELOAD,
                    shutdown.clone(),
                )),
                shutdown.clone(),
            );
        }
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
                    expiry_warner,
                    Duration::from_secs(secs),
                    shutdown.clone(),
                )),
                shutdown.clone(),
            );
        }
    }

    // 每 IP 上限、網段允許清單與 mTLS 時的兩階段計數（審計 A7）
    let per_ip_policy = PerIpPolicy::from_config(&config).map_err(config_error)?;
    if let Some(warning) = config.allowlisted_limit_warning() {
        warn!("{}", warning);
    }
    info!("{}", per_ip_policy.describe());
    let throttle = Arc::new(LogThrottle::default());

    // 註冊通道（8444）與線上 PKI（ENROLL.md、PKI.md）：需要 SQLite
    if config.provenance.source(settings::KEY_MODE) == config::SettingSource::Default {
        info!(
            "資訊 KRS-I2010：設定檔沒有 [enroll] 的 mode，註冊端點（8444）預設為關閉；需要自動註冊時設定 enroll.mode = \"open\" 或 \"renew_only\"（主控台或 kairos enroll mode 也可以修改）"
        );
    }
    // 稽核雜湊鏈的 HMAC 金鑰（data/audit-hmac.key）：只由服務建立，之後的稽核紀錄都帶 HMAC
    if config.storage.backend == StorageBackend::Sqlite
        && let Err(e) = audit::ensure_key(&paths.data_dir)
    {
        error!(
            event = "audit.key_unavailable", code = audit::CODE_CHAIN_BROKEN;
            "錯誤 {}：無法建立或讀取稽核 HMAC 的金鑰檔（稽核紀錄不會帶 HMAC，完整性驗證會失敗）：{e}",
            audit::CODE_CHAIN_BROKEN
        );
    }
    let enroll = match config.storage.backend {
        StorageBackend::Jsonl => None,
        StorageBackend::Sqlite => {
            let mut other_listens = vec![
                listener
                    .local_addr()
                    .map_err(|e| Error::new(e.kind(), format!("無法取得監聽位址: {e}")))?,
            ];
            if config.console.enabled
                && let Ok(addrs) = tokio::net::lookup_host(config.console.listen.trim()).await
            {
                other_listens.extend(addrs);
            }
            match enroll::start(enroll::StartParams {
                config: Arc::new(config.clone()),
                data_dir: paths.data_dir.clone(),
                db_path: enroll::db_path(&config, &paths.data_dir),
                shutdown: shutdown.clone(),
                other_listens,
                clock: enroll::system_clock(),
            })
            .await
            {
                Ok(r) => Some(r),
                Err(e) => {
                    error!("註冊服務無法啟動（8444 不監聽、不簽發；既有主機照常連線）：{e}");
                    None
                }
            }
        }
    };
    let enroll_service = enroll.as_ref().map(|r| r.service.clone());
    // 稽核：定期外送鏈尾雜湊與自我驗證（失敗時告警引擎開「稽核日誌完整性」）
    if let Some(svc) = &enroll_service {
        tokio::spawn(audit_monitor::run(
            svc.db.clone(),
            registry.bus(),
            config.audit.clone(),
            shutdown.clone(),
        ));
    }

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
        report_limiter: Arc::new(ReportLimiter::with_policy(per_ip_policy.clone())),
        enroll: enroll_service.clone(),
        assets,
        privacy: privacy.clone(),
    });
    let per_ip = PerIpLimiter::with_policy(per_ip_policy);

    // 告警引擎（kairos/ALERTS.md）：需要 SQLite；在自己的執行緒上執行
    let agent_addr_for_alerts = listener.local_addr().ok();
    let alerts = match (config.alerts.enabled, config.storage.backend) {
        (false, _) => {
            info!("告警引擎已停用（alerts.enabled = false）");
            None
        }
        (true, StorageBackend::Jsonl) => {
            warn!(
                "告警引擎需要 SQLite（規則、事件與靜音存在資料庫中），storage.backend = \"jsonl\" 時不啟動"
            );
            None
        }
        (true, StorageBackend::Sqlite) => {
            match alerts::AlertService::start(alerts::StartParams {
                config: Arc::new(config.clone()),
                data_dir: paths.data_dir.clone(),
                db_path: enroll::db_path(&config, &paths.data_dir),
                registry: registry.clone(),
                write_stats: Some(ctx.reports.stats().clone()),
                per_ip: Some(per_ip.clone()),
                tls: Some(tls_state.clone()),
                enroll: enroll_service.clone(),
                agent_listen: agent_addr_for_alerts,
                shutdown: shutdown.clone(),
                enroll_closed_default: config.provenance.source(settings::KEY_MODE)
                    == config::SettingSource::Default,
                clock: alerts_clock,
            })
            .await
            {
                Ok(started) => Some(started),
                Err(e) => {
                    error!("告警引擎無法啟動（主機與系統事件不會產生告警，也不會發送通知）：{e}");
                    None
                }
            }
        }
    };
    let alert_service = alerts.as_ref().map(|(svc, _)| svc.clone());
    // 健康檢查與 Prometheus（審計 O3；crate::probe）：主控台的監聽上提供，另可以純 HTTP 監聽
    let obs = Arc::new(
        probe::ObsContext::new(probe::ObsParams {
            config: Arc::new(config.clone()),
            registry: registry.clone(),
            reports: ctx.reports.clone(),
            per_ip: per_ip.clone(),
            tls: tls_state.clone(),
            enroll: enroll_service.clone(),
            shutdown: shutdown.clone(),
            db_path: (config.storage.backend == StorageBackend::Sqlite)
                .then(|| store::sqlite::SqliteOptions::from_config(&config, &paths.data_dir).path),
            alerts: alert_service.clone(),
        })
        .map_err(config_error)?,
    );
    // `/metrics` 的主機計數：訂閱內部事件匯流排（與告警引擎相同的事件，不在連線的程式中重複計數）
    tokio::spawn(metrics::watch_bus(registry.bus(), shutdown.clone()));
    if let Some(svc) = &alert_service {
        tokio::spawn(metrics::watch_alerts(svc.subscribe(), shutdown.clone()));
    }
    let probe_server = match config.observability.listen.trim() {
        "" => None,
        addr => {
            let resolved: Vec<std::net::SocketAddr> = tokio::net::lookup_host(addr)
                .await
                .map(|a| a.collect())
                .unwrap_or_default();
            let agent_addr = listener
                .local_addr()
                .map_err(|e| Error::new(e.kind(), format!("無法取得監聽位址: {e}")))?;
            if resolved
                .iter()
                .any(|a| config::listen_addrs_conflict(a, &agent_addr))
            {
                return Err(config_error(config::listen_conflict_between(
                    "server.listen",
                    &config.server.listen,
                    "observability.listen",
                    addr,
                )));
            }
            let server = probe::start_listener(obs.clone(), addr).map_err(listen_error)?;
            info!(
                "健康檢查與 Prometheus 端點：http://{}/healthz、/readyz、/metrics（{}）",
                server.local_addr,
                if obs.metrics_token.is_some() {
                    "/metrics 需要 bearer token"
                } else {
                    "/metrics 只接受 loopback 來源"
                }
            );
            Some(server)
        }
    };

    // 主控台 API（kairos/API.md）：需要 SQLite（API token 存在資料庫中）
    let console = match (config.console.enabled, config.storage.backend) {
        (false, _) => {
            info!("主控台 API 已停用（console.enabled = false）");
            None
        }
        (true, StorageBackend::Jsonl) => {
            warn!(
                "主控台 API 需要 SQLite（API token 與主機清冊存在資料庫中），storage.backend = \"jsonl\" 時不啟動；不需要主控台時可設定 console.enabled = false"
            );
            None
        }
        (true, StorageBackend::Sqlite) => {
            let agent_addr = listener
                .local_addr()
                .map_err(|e| Error::new(e.kind(), format!("無法取得監聽位址: {e}")))?;
            let started = api::start(api::StartParams {
                config: Arc::new(config.clone()),
                config_file: config_path
                    .map(Path::to_path_buf)
                    .or_else(|| default_file.is_file().then(|| default_file.to_path_buf())),
                registry: registry.clone(),
                write_stats: ctx.reports.stats().clone(),
                agent_tls: tls_state.clone(),
                per_ip: Some(per_ip.clone()),
                data_dir: paths.data_dir.clone(),
                shutdown: shutdown.clone(),
                agent_listen: agent_addr,
                enroll: enroll_service.clone(),
                alerts: alert_service.clone(),
                directory: None,
                obs: Some(obs.clone()),
                purger: purger.clone(),
            })
            .await;
            match started {
                Ok(c) => Some(c),
                Err(api::StartError::Certificate(e)) => return Err(cert_error(e)),
                Err(api::StartError::Listen(e)) => return Err(listen_error(e)),
                Err(api::StartError::Other(e)) if e.kind() == ErrorKind::InvalidInput => {
                    return Err(config_error(e));
                }
                Err(api::StartError::Other(e)) => return Err(e),
            }
        }
    };

    info!(
        "Kairos 已啟動，監聽 {}（連線上限 {}，其中握手中 {}，每個來源 IP {}，狀態表上限 {}，資料目錄 {}）",
        local_addr,
        config.server.max_connections,
        handshake_limit(config.server.max_connections),
        config.max_connections_per_ip(),
        config.max_agents(),
        config.storage.data_dir
    );

    registry.bus().publish(bus::EventKind::Lifecycle {
        phase: bus::Lifecycle::Started,
    });
    // 稽核：系統事件（啟動）
    system_audit(enroll_service.as_ref(), "system.start", config_path).await;

    obs.agent_listening
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let connections =
        accept_loop(listener, ctx.clone(), config.server.max_connections, per_ip).await;
    obs.agent_listening
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let mut connections = connections?;
    registry.bus().publish(bus::EventKind::Lifecycle {
        phase: bus::Lifecycle::ShuttingDown,
    });

    // ---- 關機 ----
    // 所有階段共用以 T0（收到訊號的時間）起算的期限
    let t0 = shutdown.started_at().unwrap_or_else(Instant::now);
    info!("已停止接受新連線");
    system_audit(enroll_service.as_ref(), "system.shutdown", config_path).await;
    // 主控台：停止接受並中斷所有連線（含 SSE）
    if let Some(console) = console {
        let _ = timeout_at(tokio_deadline(t0 + CONNECTIONS_DEADLINE), console.join()).await;
    }
    // 註冊端點：停止監聽，進行中的請求最多 5 秒
    if let Some(enroll) = enroll {
        let _ = timeout_at(tokio_deadline(t0 + CONNECTIONS_DEADLINE), enroll.join()).await;
    }
    // 探針的純 HTTP 監聽（自己的執行緒，關機時停止 accept）
    if let Some(server) = probe_server {
        let _ = timeout_at(
            tokio_deadline(t0 + CONNECTIONS_DEADLINE),
            tokio::task::spawn_blocking(move || server.join()),
        )
        .await;
    }

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
    // 告警引擎：寫入最後的事件與「正常關機」的心跳（下次啟動時判斷停機期間）
    drop(alert_service);
    if let Some((_, thread)) = alerts {
        alerts::AlertService::join(thread, Duration::from_secs(3)).await;
    }

    // 最後一份狀態（所有 agent 已標為離線）交給 writer，結束前寫入主機清冊
    if let Some(feed) = &inventory {
        feed.send_replace(Some(Arc::new(registry.snapshot(chrono::Utc::now()))));
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
        // D4：Ubuntu 預設 soft 1024、範例設定 max_connections = 1024（停用主控台時與 0.3 相同）
        let message = fd_limit_warning(1024, 1024, 0, 0).unwrap();
        assert_eq!(
            message,
            "可開啟的檔案數上限（RLIMIT_NOFILE soft limit）為 1024，低於 server.max_connections（1024）加上預留的 64（共 1088）；連線數接近上限時可能 accept 失敗（EMFILE）。建議：以 systemd 執行時在 unit 的 [Service] 加上 LimitNOFILE=4096，手動執行時先執行 ulimit -n 4096；或將 server.max_connections 調低至 960 以下（= 目前上限 1024 − 預留 64）"
        );
        // D21：主控台的連線上限計入需要量
        let message = fd_limit_warning(1100, 1024, 64, 0).unwrap();
        assert!(message.contains("console.max_connections 64"), "{message}");
        assert!(message.contains("（共 1152）"), "{message}");
        assert!(message.contains("調低至 972 以下"), "{message}");
        assert_eq!(fd_limit_warning(1152, 1024, 64, 0), None);
        // 足夠時不警告
        assert_eq!(fd_limit_warning(1088, 1024, 0, 0), None);
        assert_eq!(fd_limit_warning(u64::MAX, 65535, 4096, 0), None);
        // 需要量較大時建議值隨之提高；soft 小於預留量時不建議調低 max_connections
        let message = fd_limit_warning(32, 10_000, 64, 0).unwrap();
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
        // 多個測試同時執行：主控台使用作業系統分配的連接埠
        c.console.listen = "127.0.0.1:0".into();
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

        // 其他啟動失敗（資料目錄無法建立：路徑是一個檔案）：結束代碼 1
        let (opts, _) = run_options(&dir);
        let mut bad = run_config(&dir);
        std::fs::write(dir.join("not-a-dir"), b"x").unwrap();
        bad.storage.data_dir = dir.join("not-a-dir").to_string_lossy().into_owned();
        let result = run(bad, None, None, opts).await;
        assert!(result.is_err());
        assert_eq!(exit_code(&result), EXIT_GENERAL);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_classifies_certificate_and_listen_errors() {
        // cli-style.md §8：伺服器憑證、私鑰、客戶端 CA、CRL 的問題為 4；連接埠被佔用為 11
        let dir = std::env::temp_dir().join(format!(
            "kairos-run-codes-{}-{}",
            std::process::id(),
            next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
        std::fs::write(dir.join("not-pem.txt"), b"not a pem file\n").unwrap();
        std::fs::write(dir.join("ca.pem"), test_pki::CA).unwrap();
        let other_key = concat!(env!("CARGO_MANIFEST_DIR"), "/../pulse/testdata/server.key");

        // (情境, 修改設定)
        let mut cases: Vec<(&str, AppConfig)> = Vec::new();
        let mut c = run_config(&dir);
        c.server.cert_path = path("missing.pem");
        cases.push(("伺服器憑證不存在", c));
        let mut c = run_config(&dir);
        c.server.key_path = path("not-pem.txt");
        cases.push(("私鑰不是 PEM", c));
        let mut c = run_config(&dir);
        c.server.key_path = other_key.into();
        cases.push(("私鑰與憑證不成對", c));
        let mut c = run_config(&dir);
        c.auth.client_ca = path("missing.pem");
        cases.push(("客戶端 CA 不存在", c));
        let mut c = run_config(&dir);
        c.auth.client_ca = path("ca.pem");
        c.auth.crl = vec![path("missing.pem")];
        cases.push(("CRL 不存在", c));
        for (what, config) in cases {
            let (opts, _) = run_options(&dir);
            let result = run(config, None, None, opts).await;
            assert!(result.is_err(), "{what}: {result:?}");
            assert_eq!(exit_code(&result), EXIT_CERT, "{what}: {result:?}");
        }

        // 連接埠已被佔用
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = run_config(&dir);
        config.server.listen = occupied.local_addr().unwrap().to_string();
        let (opts, _) = run_options(&dir);
        let result = run(config, None, None, opts).await;
        let err = result.as_ref().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AddrInUse, "{err}");
        assert!(err.to_string().contains("無法監聽"), "{err}");
        assert_eq!(exit_code(&result), EXIT_CONFLICT);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn classified_errors_keep_kind_and_message() {
        let e = config_error(Error::new(ErrorKind::InvalidInput, "設定值超出範圍"));
        assert_eq!(classified_code(&e), Some(EXIT_CONFIG));
        assert_eq!(e.kind(), ErrorKind::InvalidInput);
        assert_eq!(e.to_string(), "設定值超出範圍");
        assert_eq!(exit_code(&Err(e)), EXIT_CONFIG);

        let e = cert_error(Error::new(ErrorKind::NotFound, "找不到憑證"));
        assert_eq!(e.kind(), ErrorKind::NotFound);
        assert_eq!(e.to_string(), "找不到憑證");
        assert_eq!(exit_code(&Err(e)), EXIT_CERT);

        // 只有「連接埠被佔用」標記為衝突
        assert_eq!(
            exit_code(&Err(listen_error(Error::from(ErrorKind::AddrInUse)))),
            EXIT_CONFLICT
        );
        assert_eq!(
            exit_code(&Err(listen_error(Error::from(ErrorKind::AddrNotAvailable)))),
            EXIT_GENERAL
        );

        // 未標記的錯誤：1
        assert_eq!(classified_code(&Error::other("x")), None);
        assert_eq!(classified_code(&Error::from(ErrorKind::NotFound)), None);
        assert_eq!(exit_code(&Err(Error::other("x"))), EXIT_GENERAL);
        assert_eq!(exit_code(&Ok(Some("內部錯誤".into()))), EXIT_GENERAL);
        assert_eq!(exit_code(&Ok(None)), 0);
    }

    #[test]
    fn unit_does_not_restart_when_retrying_cannot_help() {
        // kairos.service 的 RestartPreventExitStatus 必須恰好是用法、設定、憑證錯誤（2、3、4）；
        // 一般錯誤（1）與連接埠衝突（11）仍交由 systemd 重新啟動
        let unit = include_str!("../packaging/kairos.service");
        let line = unit
            .lines()
            .find(|l| l.starts_with("RestartPreventExitStatus="))
            .expect("kairos.service 應設定 RestartPreventExitStatus");
        let codes: Vec<&str> = line["RestartPreventExitStatus=".len()..]
            .split_whitespace()
            .collect();
        let expected = [EXIT_USAGE, EXIT_CONFIG, EXIT_CERT].map(|c| c.to_string());
        assert_eq!(codes, expected, "{line}");
        assert!(unit.lines().any(|l| l.trim() == "Restart=on-failure"));
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
        let result = run(config.clone(), None, None, opts).await;
        let err = result.as_ref().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
        assert!(
            err.to_string().contains("auth.allow_unauthenticated"),
            "{err}"
        );
        // 設定錯誤：結束代碼 3，systemd 不重試
        assert_eq!(exit_code(&result), EXIT_CONFIG);
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
