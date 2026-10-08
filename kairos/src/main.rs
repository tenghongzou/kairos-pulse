mod auth;
mod config;
mod connection;
mod logger;
mod monitor;
mod protocol;
mod registry;
mod reload;
mod retention;
mod shutdown;
mod storage;
mod throttle;
mod tls;

use log::{debug, error, info, warn};
use std::io::Error;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};
use tokio::task::JoinSet;
use tokio::time::{MissedTickBehavior, interval, sleep, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::config::{AppConfig, CliAction, USAGE, parse_args};
use crate::connection::{ConnContext, handle_connection, is_peer_disconnect};
use crate::registry::{Registry, next_conn_id};
use crate::shutdown::{CONNECTIONS_DEADLINE, MONITOR_DEADLINE, Shutdown, WRITER_DEADLINE};
use crate::throttle::{Category, LogThrottle};
use crate::tls::{TlsPaths, TlsState};

/// accept 失敗後的等待時間（例如檔案描述符用盡）
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// 等待新連線期間檢查「是否已解除飽和」的間隔
const SATURATION_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// 將 std 的 Instant 轉成 tokio 的 Instant，供 `timeout_at` 使用
fn tokio_deadline(at: Instant) -> tokio::time::Instant {
    tokio::time::Instant::from_std(at)
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

#[tokio::main]
async fn main() -> ExitCode {
    // 日誌尚未初始化前的錯誤只能輸出到 stderr
    let config_path = match parse_args(std::env::args().skip(1)) {
        Ok(CliAction::Run { config_path }) => config_path,
        Ok(CliAction::Help) => {
            println!("{}", USAGE);
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
    if let Err(e) = logger::setup(level) {
        eprintln!("Kairos 啟動失敗: {}", e);
        return ExitCode::FAILURE;
    }

    match run(config, config_path.as_deref()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("Kairos 啟動失敗: {}", e);
            ExitCode::FAILURE
        }
    }
}

async fn run(config: AppConfig, config_path: Option<&Path>) -> Result<(), Error> {
    let default_file = Path::new(config::DEFAULT_CONFIG_FILE);
    match config_path {
        Some(p) => info!("設定檔: {}", p.display()),
        None if default_file.is_file() => info!("設定檔: {}", default_file.display()),
        None => info!("未找到 {}，使用程式內預設值", default_file.display()),
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
    match &tls_paths.client_ca {
        Some(ca) => info!(
            "已啟用客戶端憑證驗證（mTLS），信任 {}，CRL {} 份；agent_id 必須與憑證的 SAN 相符",
            ca,
            tls_state.current().crl_count
        ),
        None => warn!(
            "未啟用客戶端憑證驗證（auth.client_ca），任何人都能以任意 agent_id 註冊，僅建議用於本機開發"
        ),
    }

    // 準備資料目錄與 report 寫入工作（按 UTC 日期分檔，啟動時與每小時清除過期檔案）
    let paths = storage::prepare_data_dir(Path::new(&config.storage.data_dir)).await?;
    let (reports, writer_task) = storage::start_report_writer(
        paths.data_dir.clone(),
        config.storage.retention_days,
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
    let shutdown = Arc::new(Shutdown::new());
    tokio::spawn(shutdown::watch(shutdown::Signals::new()?, shutdown.clone()));

    let registry = Arc::new(Registry::new());
    let writer = tokio::spawn(writer_task);
    // monitor 要等所有連線結束後才停止，才能寫出「全部離線」的最後快照，因此使用獨立的 token
    let monitor_stop = CancellationToken::new();
    let monitor = tokio::spawn(monitor::run(
        registry.clone(),
        paths.clone(),
        monitor_stop.clone(),
    ));
    tokio::spawn(logger::run_cleanup(
        config.log.retention_days,
        shutdown.clone(),
    ));

    // TLS 熱重載與現役連線的客戶端憑證重新驗證
    match config.server.reload_interval_secs {
        0 => warn!(
            "已停用 TLS 熱重載（server.reload_interval_secs = 0）：更換憑證、CA 或 CRL 需重新啟動，現役連線的客戶端憑證也不會重新驗證"
        ),
        secs => {
            info!("每 {} 秒檢查憑證、私鑰、客戶端 CA 與 CRL 是否變更", secs);
            tokio::spawn(reload::run(
                reload::Reloader::new(tls_paths.clone(), initial_fingerprint, tls_state.clone()),
                registry.clone(),
                tls_state.clone(),
                Duration::from_secs(secs),
                shutdown.clone(),
            ));
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
    });
    let limit = Arc::new(Semaphore::new(config.server.max_connections));
    let mut connections = JoinSet::new();

    info!(
        "Kairos 已啟動，監聽 {}（連線上限 {}，資料目錄 {}）",
        local_addr, config.server.max_connections, config.storage.data_dir
    );

    let mut saturation = Saturation::new(config.server.max_connections);
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
                    warn!(
                        "已達連線上限 {}，暫停接受新連線",
                        config.server.max_connections
                    );
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
                            config.server.max_connections
                        );
                    }
                }
            }
        };
        let (socket, peer) = match accepted {
            Ok(conn) => conn,
            Err(e) => {
                // 接受連線失敗（例如檔案描述符用盡）不應讓整個伺服器結束，稍候再重試
                error!("接受連線失敗: {}", e);
                drop(permit);
                sleep(ACCEPT_RETRY_DELAY).await;
                continue;
            }
        };
        let conn_id = next_conn_id();
        debug!("[#{}] 接收到來自 {} 的連線", conn_id, peer);

        // 每條連線使用接受當下的 TLS 設定；熱重載只影響之後的新連線
        let acceptor = tls_state.current().acceptor.clone();
        let ctx = ctx.clone();
        let throttle = throttle.clone();
        connections.spawn(async move {
            // permit 在連線結束時釋放
            let _permit = permit;
            match handle_connection(ctx, acceptor, socket, peer, conn_id).await {
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

    // ---- 關機 ----
    // 所有階段共用以 T0（收到訊號的時間）起算的期限
    let t0 = shutdown.started_at().unwrap_or_else(Instant::now);
    drop(listener);
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
        Ok(Ok(())) => {}
        Ok(Err(e)) => error!("report 寫入工作異常結束: {}", e),
        Err(_) => error!(
            "report 寫入未在訊號後 {:.1} 秒內完成，佇列中的資料可能遺失",
            WRITER_DEADLINE.as_secs_f64()
        ),
    }

    // 最後寫一次 agents.json
    monitor_stop.cancel();
    match timeout_at(tokio_deadline(t0 + MONITOR_DEADLINE), monitor).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => error!("監控工作異常結束: {}", e),
        Err(_) => error!(
            "最後的 agent 快照未在訊號後 {:.1} 秒內寫完",
            MONITOR_DEADLINE.as_secs_f64()
        ),
    }

    info!(
        "Kairos 已結束（關機耗時 {:.2} 秒）",
        t0.elapsed().as_secs_f64()
    );
    Ok(())
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

    #[test]
    fn saturation_threshold_at_least_one() {
        let mut s = Saturation::new(1);
        assert!(s.enter());
        assert!(!s.check_resume(0));
        assert!(s.check_resume(1));
    }
}
