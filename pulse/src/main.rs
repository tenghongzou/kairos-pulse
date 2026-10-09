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
#[cfg(test)]
mod test_log;
mod tls;

use config_manager::AppConfig;
use log::{Level, LevelFilter, debug, error, info, warn};
use std::fs::create_dir_all;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
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
///
/// 清除前的時鐘跳躍保護狀態保存在 `retention` 中，跨次延續。
fn spawn_log_cleanup(retention: log_files::LogRetention, retention_days: u32) {
    if retention_days == 0 {
        return;
    }
    let retention = Arc::new(Mutex::new(retention));
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + LOG_CLEANUP_INTERVAL,
            LOG_CLEANUP_INTERVAL,
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let retention = retention.clone();
            if let Err(e) = tokio::task::spawn_blocking(move || {
                // 上一次清除 panic 時鎖會被標記為 poisoned；狀態本身仍可用，繼續使用
                retention
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .run()
            })
            .await
            {
                warn!("清除過期日誌檔時發生錯誤：{e}");
            }
        }
    });
}

/// 每 24 小時重新檢查一次客戶端憑證的有效期限（啟動時的檢查已在 `tls::build` 完成）。
/// 未設定客戶端憑證時不啟動。
fn spawn_client_cert_recheck(tls: tls::TlsSetup) {
    if !tls.has_client_cert() {
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + tls::CERT_RECHECK_INTERVAL,
            tls::CERT_RECHECK_INTERVAL,
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            tls.log_client_cert_validity();
        }
    });
}

/// 啟動時記錄工作目錄與設定檔來源的訊息（等級、內容）。
///
/// 設定中的相對路徑（`pulse.toml`、`agent.id`、`logs/`）都以工作目錄為準；Windows 服務的預設工作目錄
/// 是 `C:\Windows\System32`，檔案會落在那裡，因此遇到系統目錄時另外記 warn。
fn startup_location_messages(cwd: Option<&Path>, local_file_found: bool) -> Vec<(Level, String)> {
    let Some(cwd) = cwd else {
        return vec![(
            Level::Warn,
            "無法取得目前的工作目錄；設定中的相對路徑（pulse.toml、agent.id、logs/）可能不在預期的位置"
                .to_string(),
        )];
    };
    let source = if local_file_found {
        format!(
            "已讀取 {}",
            cwd.join(config_manager::LOCAL_CONFIG_FILE).display()
        )
    } else {
        format!(
            "{} 不存在，只使用內建預設值與 PULSE__* 環境變數",
            cwd.join(config_manager::LOCAL_CONFIG_FILE).display()
        )
    };
    let mut messages = vec![(
        Level::Info,
        format!(
            "工作目錄：{}（設定中的相對路徑都以此為準）；設定檔：{source}",
            cwd.display()
        ),
    )];
    let last = cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase());
    if matches!(last.as_deref(), Some("system32" | "syswow64")) {
        messages.push((
            Level::Warn,
            format!(
                "工作目錄是系統目錄 {}（以 Windows 服務執行時的預設值）：pulse.toml 需放在這裡才會被讀取，\
                 agent.id 與 logs/ 也會建立在這裡。建議在服務設定中把工作目錄設為 Pulse 所在的目錄",
                cwd.display()
            ),
        ));
    }
    messages
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
    // 不讀取設定：設定有誤時也能確認版本
    if command == cli::Command::Version {
        println!("{}", cli::VERSION);
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

    // 日誌清除的日期基準必須在初始化日誌之前決定：fern 一初始化就會建立今天的日誌檔，
    // 時鐘被誤校到未來時，那個檔案不能被當成「最新日誌檔的日期」
    let mut retention =
        log_files::LogRetention::open(Path::new(log_files::LOG_DIR), config.log.retention_days);

    // 初始化日誌
    if let Err(e) = setup_logger(config.log.level_filter()) {
        eprintln!("錯誤：無法初始化日誌：{e}");
        return ExitCode::FAILURE;
    }
    // 啟動時清除一次過期日誌檔（之後每小時一次，見 spawn_log_cleanup）
    retention.run();

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
    let code = runtime.block_on(async_main(config, retention));
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
            // 此模式不初始化日誌，警告直接寫到 stderr（stdout 只放 ID）
            if let Some(warning) = agent_id::permission_warning(&path) {
                eprintln!("警告：{warning}");
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
async fn async_main(config: AppConfig, retention: log_files::LogRetention) -> ExitCode {
    let mut signals = match shutdown::Signals::new() {
        Ok(s) => s,
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let shutdown = CancellationToken::new();
    let work = run(config, retention, shutdown.clone());
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
async fn run(
    config: AppConfig,
    retention: log_files::LogRetention,
    shutdown: CancellationToken,
) -> Result<(), String> {
    // 完整版本字串（含 commit）；hello 的 agent_version 仍只送 CARGO_PKG_VERSION
    info!("Pulse 地端程式啟動（{}）...", cli::VERSION);
    let cwd = std::env::current_dir().ok();
    for (level, msg) in startup_location_messages(cwd.as_deref(), config.local_file_found) {
        log::log!(level, "{msg}");
    }
    debug!("使用的配置: {:?}", config);

    // TLS 設定只在啟動時建立一次；憑證 / 私鑰讀不到、格式錯誤、不成對、過期都會在這裡直接失敗。
    // 讀取系統信任庫與憑證檔是阻塞 I/O（系統信任庫可能很慢），放到 blocking 執行緒，
    // 且收到 Ctrl+C 時不等它完成
    let server_config = config.server.clone();
    let Some(tls) = blocking_or_shutdown(&shutdown, move || tls::build(&server_config))
        .await
        .map_err(|e| format!("啟動失敗：{e}"))?
    else {
        return Ok(());
    };
    let tls = tls.map_err(|e| format!("啟動失敗：{e}"))?;

    // agent_id：讀檔沿用，不存在則產生；格式不合法則結束（檔案 I/O 放到 blocking 執行緒）
    let id_path = PathBuf::from(&config.agent.id_file);
    let path = id_path.clone();
    let Some((loaded, permission_warning)) = blocking_or_shutdown(&shutdown, move || {
        let loaded = agent_id::load_or_create(&path);
        (loaded, agent_id::permission_warning(&path))
    })
    .await
    .map_err(|e| format!("啟動失敗：{e}"))?
    else {
        return Ok(());
    };
    let (agent_id, created) = loaded.map_err(|e| format!("啟動失敗：{e}"))?;
    if let Some(warning) = permission_warning {
        warn!("{warning}");
    }
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

    spawn_log_cleanup(retention, config.log.retention_days);
    spawn_client_cert_recheck(tls.clone());

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_logs_working_directory_and_config_source() {
        // R2-P6
        let dir = Path::new("/opt/pulse");
        let msgs = startup_location_messages(Some(dir), true);
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert_eq!(msgs[0].0, Level::Info);
        assert!(
            msgs[0].1.contains("工作目錄")
                && msgs[0].1.contains("已讀取")
                && msgs[0].1.contains("pulse.toml"),
            "{msgs:?}"
        );
        let msgs = startup_location_messages(Some(dir), false);
        assert!(msgs[0].1.contains("不存在"), "{msgs:?}");

        // Windows 服務的預設工作目錄（大小寫不拘）→ 另外記 warn
        for sys in [
            r"C:\Windows\System32",
            r"C:\WINDOWS\system32",
            r"C:\Windows\SysWOW64",
        ] {
            let msgs = startup_location_messages(Some(Path::new(sys)), false);
            if cfg!(windows) {
                assert_eq!(msgs.len(), 2, "{sys}: {msgs:?}");
                assert_eq!(msgs[1].0, Level::Warn);
                assert!(msgs[1].1.contains("系統目錄"), "{msgs:?}");
            }
        }
        assert_eq!(
            startup_location_messages(Some(Path::new("/srv/system32/pulse")), false).len(),
            1
        );
        // 取不到工作目錄 → warn
        let msgs = startup_location_messages(None, true);
        assert_eq!(msgs[0].0, Level::Warn);
    }
}
