mod agent_id;
mod backoff;
mod cli;
mod config_manager;
mod log_files;
mod log_throttle;
mod network;
mod protocol;
mod shutdown;
mod system_info;
mod tls;

use config_manager::AppConfig;
use log::{LevelFilter, debug, error, info, warn};
use std::fs::create_dir_all;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 收集 task → 連線 task 的回報佇列容量。
///
/// 刻意設為 1：連線中斷時最多只保留一筆取樣，其餘新取樣直接丟棄，不會堆積。
const REPORT_QUEUE_CAPACITY: usize = 1;

/// 收到 Ctrl+C 後，等待連線任務收尾（含送 TLS close_notify，上限 1 秒）的時間上限。
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(1500);

/// runtime 關閉時等待仍在執行的 blocking 任務（例如啟動時的系統資訊收集）的上限；
/// 超過即不再等待，直接結束行程。
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);

/// 過期日誌檔的清除間隔。
const LOG_CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);

fn setup_logger(log_level: LevelFilter) -> Result<(), fern::InitError> {
    // 確保日誌目錄存在
    create_dir_all(log_files::LOG_DIR)?;

    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} [{}] {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                record.level(),
                message
            ))
        })
        .level(log_level)
        // 第三方套件的除錯訊息太多，只保留警告以上
        .level_for("config", LevelFilter::Warn)
        .level_for("rustls", LevelFilter::Warn)
        .chain(std::io::stdout()) // 日誌輸出到終端
        .chain(log_files::daily_output()) // 日誌輸出到 logs/pulse-YYYY-MM-DD.log（UTC 日期）
        .apply()?;

    Ok(())
}

/// 每小時清除一次過期日誌檔（啟動時的清除已在 `main` 完成）。`retention_days == 0` 時不啟動。
fn spawn_log_cleanup(retention_days: u32) {
    if retention_days == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + LOG_CLEANUP_INTERVAL,
            LOG_CLEANUP_INTERVAL,
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if let Err(e) =
                tokio::task::spawn_blocking(move || log_files::cleanup_now(retention_days)).await
            {
                warn!("清除過期日誌檔時發生錯誤：{e}");
            }
        }
    });
}

fn main() -> ExitCode {
    // 命令列參數（非 UTF-8 的參數以替換字元呈現，必然是未知參數）
    let command = match cli::parse_args(
        std::env::args_os()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned()),
    ) {
        Ok(command) => command,
        Err(e) => {
            eprintln!("錯誤：{e}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    if command == cli::Command::Help {
        println!("{}", cli::USAGE);
        return ExitCode::SUCCESS;
    }

    // 設定：嵌入預設 → pulse.toml → PULSE__* 環境變數（含數值範圍與 log.level 驗證）
    let config = match AppConfig::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("錯誤：無法載入設定：{e}");
            return ExitCode::FAILURE;
        }
    };

    if command == cli::Command::PrintAgentId {
        return print_agent_id(&config);
    }

    // 初始化日誌
    if let Err(e) = setup_logger(config.log.level_filter()) {
        eprintln!("錯誤：無法初始化日誌：{e}");
        return ExitCode::FAILURE;
    }
    // 啟動時清除一次過期日誌檔（之後每小時一次，見 spawn_log_cleanup）
    log_files::cleanup_now(config.log.retention_days);

    // 自建 runtime（而非 #[tokio::main]），以便結束時用 shutdown_timeout：
    // 不必等待仍在 blocking 執行緒上跑的系統呼叫（例如很慢的 WMI 查詢）。
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("無法建立 tokio runtime：{e}");
            return ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(async_main(config));
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    code
}

/// `--print-agent-id`：讀取（或第一次產生）agent_id，只把 ID 一行印到 stdout。
///
/// 不連線、不初始化日誌；其他說明一律寫到 stderr，方便腳本直接取用 stdout。
fn print_agent_id(config: &AppConfig) -> ExitCode {
    let path = PathBuf::from(&config.agent.id_file);
    match agent_id::load_or_create(&path) {
        Ok((id, created)) => {
            if created {
                eprintln!("已產生新的 agent_id，存放於 {}", path.display());
            }
            println!("{id}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("錯誤：{e}");
            ExitCode::FAILURE
        }
    }
}

/// 主流程與結束訊號同時監聽。
///
/// 收到結束訊號（見 [`shutdown::Signals`]）後通知各任務結束（已連線時會先送 TLS close_notify），
/// 最多等待 `GRACEFUL_SHUTDOWN_TIMEOUT`；期間再收到一次訊號就直接強制結束。
async fn async_main(config: AppConfig) -> ExitCode {
    let mut signals = match shutdown::Signals::new() {
        Ok(s) => s,
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let shutdown = CancellationToken::new();
    let work = run(config, shutdown.clone());
    tokio::pin!(work);

    let result = tokio::select! {
        res = &mut work => res,
        name = signals.recv() => {
            info!("收到 {name} 訊號，正在結束（再送一次訊號可強制結束）...");
            shutdown.cancel();
            tokio::select! {
                res = tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, &mut work) => match res {
                    Ok(res) => res,
                    Err(_) => {
                        warn!(
                            "等待任務收尾逾時（{} ms），直接結束",
                            GRACEFUL_SHUTDOWN_TIMEOUT.as_millis()
                        );
                        Ok(())
                    }
                },
                name = signals.recv() => {
                    warn!("再次收到 {name} 訊號，強制結束");
                    std::process::exit(shutdown::FORCED_EXIT_CODE);
                }
            }
        }
    };

    match result {
        Ok(()) => {
            info!("Pulse 已結束");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// 在 blocking 執行緒執行 `f`，但 `shutdown` 觸發時立即放棄等待（回傳 `None`）。
async fn blocking_or_shutdown<T, F>(shutdown: &CancellationToken, f: F) -> Result<Option<T>, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::select! {
        res = tokio::task::spawn_blocking(f) => res
            .map(Some)
            .map_err(|e| format!("背景工作異常結束：{e}")),
        _ = shutdown.cancelled() => Ok(None),
    }
}

/// 日誌初始化後的主流程。
///
/// 正常情況下會一直執行到 `shutdown` 被觸發後返回 `Ok(())`；
/// 啟動失敗或背景任務異常結束時回傳 `Err`，程式以非 0 結束。
async fn run(config: AppConfig, shutdown: CancellationToken) -> Result<(), String> {
    info!(
        "Pulse 地端程式啟動（版本 {}）...",
        env!("CARGO_PKG_VERSION")
    );
    debug!("使用的配置: {:?}", config);

    // TLS 設定只在啟動時建立一次；憑證 / 私鑰讀不到、格式錯誤、不成對都會在這裡直接失敗
    let tls = tls::build(&config.server).map_err(|e| format!("啟動失敗：{e}"))?;

    // agent_id：讀檔沿用，不存在則產生；格式不合法則結束（檔案 I/O 放到 blocking 執行緒）
    let id_path = PathBuf::from(&config.agent.id_file);
    let path = id_path.clone();
    let Some(loaded) = blocking_or_shutdown(&shutdown, move || agent_id::load_or_create(&path))
        .await
        .map_err(|e| format!("啟動失敗：{e}"))?
    else {
        return Ok(());
    };
    let (agent_id, created) = loaded.map_err(|e| format!("啟動失敗：{e}"))?;
    if created {
        info!(
            "已產生新的 agent_id {agent_id}，存放於 {}",
            id_path.display()
        );
    } else {
        info!("agent_id：{agent_id}（{}）", id_path.display());
    }
    // 客戶端憑證的 SAN 必須是 DNS:<agent_id>，否則伺服器會以 auth_failed 拒絕：啟動時就檢查
    tls.verify_client_cert_identity(&agent_id)
        .map_err(|e| format!("啟動失敗：{e}"))?;

    // 印出一次完整系統資訊。內含約 200 ms 的阻塞 sleep 與大量系統呼叫（可能很慢），
    // 放到 blocking 執行緒，且收到 Ctrl+C 時不等它完成。
    match blocking_or_shutdown(&shutdown, system_info::get_system_info).await {
        Ok(Some(())) => {}
        Ok(None) => return Ok(()),
        // 只是啟動時的資訊輸出，失敗不影響監控本身
        Err(e) => error!("收集啟動系統資訊時發生錯誤：{e}"),
    }
    let Some(hostname) = blocking_or_shutdown(&shutdown, system_info::hostname)
        .await
        .map_err(|e| format!("啟動失敗：無法取得主機名稱：{e}"))?
    else {
        return Ok(());
    };

    spawn_log_cleanup(config.log.retention_days);

    // 系統資訊收集 task → 連線 task
    let (report_tx, report_rx) = mpsc::channel(REPORT_QUEUE_CAPACITY);
    let mut collector = tokio::spawn(system_info::collect_periodically(
        Duration::from_secs(config.report.interval),
        report_tx,
    ));

    let settings = network::Settings::new(&config, agent_id, hostname);
    tokio::select! {
        _ = network::run(settings, tls, report_rx, shutdown.clone()) => {
            if shutdown.is_cancelled() {
                Ok(())
            } else {
                Err("連線任務意外結束".to_string())
            }
        }
        // 收集 task 正常情況下不會結束；結束或 panic 時讓整個程式以非 0 結束，交由服務管理員重啟
        res = &mut collector => Err(match res {
            Ok(()) => "系統資訊收集任務意外結束".to_string(),
            Err(e) if e.is_panic() => format!("系統資訊收集任務發生 panic：{e}"),
            Err(e) => format!("系統資訊收集任務異常結束：{e}"),
        }),
    }
}
