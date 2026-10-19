mod agent_id;
mod backoff;
mod cli;
mod codes;
mod config_manager;
mod decommission;
mod diag;
mod enroll;
mod file_perms;
mod inventory;
mod local_status;
mod log_files;
mod log_throttle;
mod metrics;
mod network;
mod protocol;
mod remote;
mod rotation;
mod server_addr;
mod service_status;
mod shutdown;
mod spool;
mod status_cmd;
mod status_file;
mod system_info;
#[cfg(test)]
mod test_log;
mod tls;
mod winsvc;

use config_manager::AppConfig;
use log::{Level, LevelFilter, debug, error, info, warn};
use std::fs::create_dir_all;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 收集 task → 連線 task 的回報佇列容量（at-most-once 的即時取樣）。
///
/// 刻意設為 1：連線中斷時最多只保留一筆取樣，其餘新取樣直接丟棄，不會堆積。
/// 啟用本機緩衝時，斷線期間的取樣寫進緩衝（`spool`），不經過這個佇列。
const REPORT_QUEUE_CAPACITY: usize = 1;

/// 啟用本機緩衝時，收集 task → 緩衝寫入 task 的佇列容量（寫入 task 一直在讀，
/// 只需容納 fsync 等短暫的延遲）
const SPOOL_QUEUE_CAPACITY: usize = 8;

/// 收到 Ctrl+C 後，等待連線任務收尾（含送 TLS close_notify，上限 1 秒）的時間上限。
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(1500);

/// runtime 關閉時等待仍在執行的 blocking 任務（例如啟動時的系統資訊收集）的上限；
/// 超過即不再等待，直接結束行程。
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);

/// 過期日誌檔的清除間隔。
const LOG_CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);

// ---- 結束代碼 ----
//
// 依設計包 `design/pulse/cli-spec.md` §1.3（`pulse`／`pulse run` 常駐）與
// `design/design-system/cli-style.md` §8（`kairos` 與 `pulse` 共用的結束代碼表）。
// 另有正常結束 0、第二次收到結束訊號時的 130（`shutdown::FORCED_EXIT_CODE`）。不使用 sysexits。
// `packaging/pulse.service` 以 `RestartPreventExitStatus` 讓 systemd 不重試「重試也不會成功」的
// 代碼（2、3、4）。

/// 一般錯誤：內部錯誤（收集任務異常結束等）與未分類的啟動失敗（例如日誌目錄無法建立、
/// 無法取得主機名稱），交由服務管理員重新啟動
const EXIT_GENERAL: u8 = 1;
/// 用法錯誤：未知的命令列參數
const EXIT_USAGE: u8 = 2;
/// 設定錯誤（`AppConfig::load` 失敗：設定檔語法、型別、範圍錯誤等）
const EXIT_CONFIG: u8 = 3;
/// 憑證錯誤：`server.ca_cert`、客戶端憑證或私鑰無法讀取、格式錯誤、不成對，
/// 客戶端憑證的 SAN 與 agent_id 不符，或 `agent.id` 無法讀取、內容不合法
const EXIT_CERT: u8 = 4;

/// 啟動失敗或異常結束的原因與對應的結束代碼
#[derive(Debug, PartialEq, Eq)]
struct Failure {
    code: u8,
    message: String,
    /// 對應的錯誤代碼（`agent.fatal` 日誌的 code 欄；無法歸類時為 `None`）
    pls: Option<&'static str>,
}

impl Failure {
    /// 憑證錯誤（[`EXIT_CERT`]）
    fn cert(message: String) -> Self {
        Self {
            code: EXIT_CERT,
            message,
            pls: None,
        }
    }

    /// 附上錯誤代碼
    fn pls(mut self, code: &'static str) -> Self {
        self.pls = Some(code);
        self
    }
}

/// 未分類的錯誤訊息一律是一般錯誤（[`EXIT_GENERAL`]）
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            code: EXIT_GENERAL,
            message,
            pls: None,
        }
    }
}

/// 初始化日誌：`logs/pulse-YYYY-MM-DD.log`（`log.format = "json"` 時為 `.jsonl`，UTC 日期），
/// 可選擇同時寫 stdout（`log.stdout`）與 syslog（`[log.syslog]`）。行的格式見 `kp_obs::logfmt`
fn setup_logger(config: &config_manager::LogConfig) -> Result<(), String> {
    // 確保日誌目錄存在
    create_dir_all(log_files::LOG_DIR).map_err(|e| e.to_string())?;
    let host = kp_obs::logfmt::hostname();
    let syslog = match config.syslog_config(&host)? {
        Some(c) => Some(
            kp_obs::syslog::Sender::start(c).map_err(|e| format!("無法啟動 syslog 外送：{e}"))?,
        ),
        None => None,
    };
    let format = config.log_format();
    let meta = kp_obs::logfmt::LogMeta::new("pulse", host, true);
    // 多實例（pulse-N.service 的 StateDirectory=pulse/N）：JSON 日誌的 instance 為 N
    meta.set_instance(
        status_file::state_directory(std::env::var_os("STATE_DIRECTORY").as_deref())
            .and_then(|d| status_file::instance_of(&d)),
    );
    let sink = Arc::new(kp_obs::logfmt::LogSink::new(
        Arc::new(meta),
        format,
        config.log_timezone(),
        Box::new(log_files::daily_file(format)),
        config.stdout,
        syslog,
    ));
    log_files::set_sink(sink.clone());

    fern::Dispatch::new()
        // 等級由 log 的 max_level 控制（集中設定可即時改變 log.level，REMOTE.md 第 3.1 節）
        .level(LevelFilter::Trace)
        // 第三方套件的除錯訊息太多，只保留警告以上
        .level_for("config", LevelFilter::Warn)
        .level_for("rustls", LevelFilter::Warn)
        .chain(fern::Output::call(move |record| sink.log(record)))
        .apply()
        .map_err(|e| e.to_string())?;
    log::set_max_level(config.level_filter());
    Ok(())
}

/// 每小時清除一次過期日誌檔（啟動時的清除已在 `main` 完成），並套用集中設定即時改變的 `log.level`、
/// `log.retention_days`（REMOTE.md 第 3.1 節：日誌等級立即生效，保留天數在下一次清除生效）。
///
/// 清除前的時鐘跳躍保護狀態保存在 `retention` 中，跨次延續；保留天數改變時以新的天數重新開啟。
fn spawn_live_logging(
    mut live: tokio::sync::watch::Receiver<Arc<AppConfig>>,
    retention: log_files::LogRetention,
    retention_days: u32,
) {
    let retention = Arc::new(Mutex::new(retention));
    tokio::spawn(async move {
        let mut days = retention_days;
        let mut level = live.borrow().log.level_filter();
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + LOG_CLEANUP_INTERVAL,
            LOG_CLEANUP_INTERVAL,
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut live_open = true;
        loop {
            let run_now = tokio::select! {
                _ = ticker.tick() => days > 0,
                changed = live.changed(), if live_open => {
                    if changed.is_err() {
                        live_open = false;
                        continue;
                    }
                    let (new_level, new_days) = {
                        let c = live.borrow_and_update();
                        (c.log.level_filter(), c.log.retention_days)
                    };
                    if new_level != level {
                        log::set_max_level(new_level);
                        info!("日誌等級改為 {new_level}（集中設定）");
                        level = new_level;
                    }
                    if new_days == days {
                        continue;
                    }
                    info!("日誌保留天數改為 {new_days}（集中設定；0 = 永久保留）");
                    days = new_days;
                    let r = retention.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        *r.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                            log_files::LogRetention::open(Path::new(log_files::LOG_DIR), new_days);
                    })
                    .await;
                    days > 0
                }
            };
            if !run_now {
                continue;
            }
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
                "{}工作目錄是系統目錄 {}（以 Windows 服務執行時的預設值）：pulse.toml 需放在這裡才會被讀取，\
                 agent.id 與 logs/ 也會建立在這裡。建議在服務設定中把工作目錄設為 Pulse 所在的目錄",
                codes::tag(codes::CONFIG_WARNING, Some(codes::W_SYSTEM32_WORKDIR)),
                cwd.display()
            ),
        ));
    }
    messages
}

/// 設定類警告（寫進狀態檔，`pulse status` 的「設定」列）：停用伺服器憑證驗證（`PLS-W0302`）、
/// 同時信任系統信任庫與私有 CA（`PLS-W0303`）
fn config_warnings(config: &AppConfig) -> Vec<status_file::CodeNote> {
    let mut out = Vec::new();
    if config.server.accept_invalid_certs {
        out.push(status_file::CodeNote::new(
            codes::W_TLS_VERIFY_DISABLED,
            "已停用伺服器憑證驗證（accept_invalid_certs = true）",
        ));
    } else if !config.server.ca_cert.trim().is_empty() && config.server.trusts_system_roots() {
        out.push(status_file::CodeNote::new(
            codes::W_BOTH_TRUST_STORES,
            "同時信任系統信任庫與 server.ca_cert（use_system_roots = true）",
        ));
    }
    out
}

/// 建立狀態追蹤器（`status.file` 以工作目錄解析；沿用上一個行程留下的狀態檔計算最近 1 小時的重啟次數）
fn status_tracker(config: &AppConfig, cwd: Option<&Path>) -> local_status::Tracker {
    let base = cwd.map(Path::to_path_buf).unwrap_or_default();
    // status.file 未設定：設計包的位置（/var/lib/pulse、%ProgramData%\Pulse），無法寫入時改寫 logs/
    // systemd 的 StateDirectory=（多實例為 pulse/N）會設定 STATE_DIRECTORY，優先使用
    let (path, fallback) = match config.status.file.trim() {
        "" => (
            status_file::state_directory(std::env::var_os("STATE_DIRECTORY").as_deref())
                .map_or_else(status_file::default_path, |d| d.join("status.json")),
            Some(base.join(status_file::LEGACY_RELATIVE)),
        ),
        file => (base.join(file), None),
    };
    let previous = status_file::read(&path)
        .ok()
        .or_else(|| fallback.as_ref().and_then(|f| status_file::read(f).ok()));
    let config_file = config.local_file_found.then(|| {
        base.join(config_manager::LOCAL_CONFIG_FILE)
            .display()
            .to_string()
    });
    let address = config.server.address.trim();
    let server = if address.contains(':') && !address.starts_with('[') {
        format!("[{address}]:{}", config.server.port)
    } else {
        format!("{address}:{}", config.server.port)
    };
    let mut file = local_status::initial_file(
        previous.as_ref(),
        config_file,
        cwd.map(|c| c.display().to_string()),
        Duration::from_secs(config.report.interval),
        server,
        Some(config.agent.display_name.trim().to_string()).filter(|n| !n.is_empty()),
    );
    file.config.warnings = config_warnings(config);
    // 候選位址的數量（server.address 加 server.fallback_addresses；kairos/HA.md 第 8.7 節）
    let candidates = config.server.candidates().len();
    file.connection.candidates = u32::try_from(candidates).ok();
    metrics::global().set_candidate_count(candidates);
    file.identity.agent_id_file =
        Some(base.join(config.agent.id_file.trim()).display().to_string());
    let buffer = local_status::BufferSettings {
        dir: config
            .buffer
            .enabled
            .then(|| base.join(config.buffer.dir.trim())),
        max_bytes: config.buffer.max_mb.saturating_mul(1024 * 1024),
        max_records: config.buffer.max_records,
        max_age_hours: config.buffer.max_age_hours,
        replay_interval_ms: config.effective_replay_interval_ms(),
    };
    local_status::Tracker::new(path, fallback, file, buffer)
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
            eprintln!("錯誤：{}\n\n{}", e.message, e.usage);
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match &command {
        cli::Command::Help => {
            println!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        cli::Command::StatusHelp => {
            println!("{}", cli::STATUS_USAGE);
            return ExitCode::SUCCESS;
        }
        // pulse status：只讀狀態檔與服務管理員，不初始化日誌、不連線（cli-spec §4）
        cli::Command::Status(args) => return ExitCode::from(status_cmd::run(args)),
        cli::Command::Explain(code) => return ExitCode::from(status_cmd::explain(code)),
        cli::Command::EnrollHelp => {
            println!("{}", enroll::cli::USAGE);
            return ExitCode::SUCCESS;
        }
        cli::Command::DecommissionHelp => {
            println!("{}", enroll::cli::DECOMMISSION_USAGE);
            return ExitCode::SUCCESS;
        }
        // pulse enroll／decommission：自行讀取設定（-c），不初始化日誌檔
        cli::Command::Enroll(args) => return ExitCode::from(enroll::cli::run(args)),
        cli::Command::Decommission(args) => {
            return ExitCode::from(enroll::cli::run_decommission(args));
        }
        // pulse check-config／diag：自行讀取設定（-c），不初始化日誌檔、不連線
        cli::Command::CheckConfigHelp => {
            println!("{}", diag::CHECK_USAGE);
            return ExitCode::SUCCESS;
        }
        cli::Command::DiagHelp => {
            println!("{}", diag::DIAG_USAGE);
            return ExitCode::SUCCESS;
        }
        cli::Command::CheckConfig(args) => return ExitCode::from(diag::run_check(args.clone())),
        cli::Command::Diag(args) => return ExitCode::from(diag::run_diag(args.clone())),
        cli::Command::ServiceHelp => {
            println!("{}", winsvc::USAGE);
            return ExitCode::SUCCESS;
        }
        // pulse service install|uninstall|run（Windows 服務；其他平台以用法錯誤結束）
        cli::Command::Service(args) => return ExitCode::from(winsvc::run(args)),
        // pulse config show／pulse upgrade …（遠端管理；REMOTE.md 第 10.2 節）
        cli::Command::ConfigHelp => {
            println!("{}", remote::cli::CONFIG_USAGE);
            return ExitCode::SUCCESS;
        }
        cli::Command::Config(args) => return ExitCode::from(remote::cli::run_config(args)),
        cli::Command::UpgradeHelp => {
            println!("{}", remote::cli::UPGRADE_USAGE);
            return ExitCode::SUCCESS;
        }
        cli::Command::Upgrade(args) => return ExitCode::from(remote::cli::run_upgrade(args)),
        _ => {}
    }
    // 不讀取設定：設定有誤時也能確認版本
    if command == cli::Command::Version {
        println!("{}", cli::VERSION);
        return ExitCode::SUCCESS;
    }
    ExitCode::from(run_agent(command == cli::Command::PrintAgentId, None))
}

/// 讀取設定、初始化日誌後執行監控代理（`pulse`／`pulse run`），或 `--print-agent-id`。
///
/// 設定檔與相對路徑以目前目錄解析：以 Windows 服務執行時，[`winsvc`] 先把目前目錄切換到安裝目錄
/// （或 `--workdir`），不會落在 `System32`。`external_stop` 是服務控制管理員的停止要求（Windows
/// 服務）；`None` 時只依結束訊號（SIGINT／SIGTERM、Ctrl+C 等）結束。回傳結束代碼
fn run_agent(print_id: bool, external_stop: Option<CancellationToken>) -> u8 {
    // 設定：嵌入預設 → pulse.toml → PULSE__* 環境變數（含數值範圍與 log.level 驗證）
    let config = match AppConfig::load() {
        Ok(config) => config,
        Err(e) => {
            // 檔案存在但無法讀取（權限、I/O）：PLS-E0402（與 pulse check-config 相同；設計包 §28.5）
            let local = Path::new(config_manager::LOCAL_CONFIG_FILE);
            let code = if local.is_file() && std::fs::File::open(local).is_err() {
                codes::E_CONFIG_UNREADABLE
            } else {
                match e {
                    config::ConfigError::Message(_) => codes::E_CONFIG_RANGE,
                    _ => codes::E_CONFIG_SYNTAX,
                }
            };
            eprintln!("錯誤 {code}：無法載入設定：{e}");
            // 重試也不會成功：以 EXIT_CONFIG 結束，systemd 不重新啟動（RestartPreventExitStatus）
            return EXIT_CONFIG;
        }
    };

    if print_id {
        return print_agent_id(&config);
    }

    // 日誌清除的日期基準必須在初始化日誌之前決定：fern 一初始化就會建立今天的日誌檔，
    // 時鐘被誤校到未來時，那個檔案不能被當成「最新日誌檔的日期」
    let mut retention =
        log_files::LogRetention::open(Path::new(log_files::LOG_DIR), config.log.retention_days);

    // 初始化日誌
    if let Err(e) = setup_logger(&config.log) {
        eprintln!("錯誤 {}：無法初始化日誌：{e}", codes::E_LOG_INIT);
        return EXIT_GENERAL;
    }
    // 啟動時清除一次過期日誌檔（之後每小時一次，見 spawn_live_logging）
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
            return EXIT_GENERAL;
        }
    };
    let code = runtime.block_on(async_main(config, retention, external_stop));
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    log_files::flush_all();
    code
}

/// `--print-agent-id`：讀取（或第一次產生）agent_id，只把 ID 一行印到 stdout。
///
/// 不連線、不初始化日誌；其他說明一律寫到 stderr，方便腳本直接取用 stdout。
fn print_agent_id(config: &AppConfig) -> u8 {
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
            0
        }
        Err(e) => {
            eprintln!("錯誤：{e}");
            EXIT_GENERAL
        }
    }
}

/// 主流程與結束訊號同時監聽。
///
/// 收到結束訊號（見 [`shutdown::Signals`]）後通知各任務結束（已連線時會先送 TLS close_notify），
/// 最多等待 `GRACEFUL_SHUTDOWN_TIMEOUT`；期間再收到一次訊號就直接強制結束。
async fn async_main(
    config: AppConfig,
    retention: log_files::LogRetention,
    external_stop: Option<CancellationToken>,
) -> u8 {
    // 以 Windows 服務執行時沒有主控台：無法監聽 Ctrl+C 等事件時只依服務控制管理員的停止要求結束
    let mut signals = match (shutdown::Signals::new(), &external_stop) {
        (Ok(s), _) => Some(s),
        (Err(e), Some(_)) => {
            debug!("{e}（以服務執行，改由服務控制管理員的停止要求結束）");
            None
        }
        (Err(e), None) => {
            error!("{e}");
            return EXIT_GENERAL;
        }
    };
    let external = external_stop.unwrap_or_default();
    // 第二次結束要求只看結束訊號（服務的停止要求只有一次）
    let never = CancellationToken::new();
    let shutdown = CancellationToken::new();
    let work = run(config, retention, shutdown.clone());
    tokio::pin!(work);

    let result = tokio::select! {
        res = &mut work => res,
        name = next_signal(&mut signals, &external) => {
            info!("收到 {name}，正在結束（再送一次訊號可強制結束）...");
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
                name = next_signal(&mut signals, &never) => {
                    warn!("再次收到 {name}，強制結束");
                    std::process::exit(shutdown::FORCED_EXIT_CODE);
                }
            }
        }
    };

    match result {
        Ok(()) => {
            if let Some(t) = local_status::tracker() {
                t.write_final(0, None);
            }
            info!("{}Pulse 已結束", codes::tag(codes::AGENT_STOP, None));
            0
        }
        Err(failure) => {
            if let Some(t) = local_status::tracker() {
                t.write_final(i32::from(failure.code), Some(&failure.message));
            }
            error!(
                "{}{}",
                codes::tag(codes::AGENT_FATAL, failure.pls),
                failure.message
            );
            // 日誌只寫到 stdout 與 logs/（pulse.service 為 StandardOutput=null）；以非 0 結束的原因
            // 另寫一行到 stderr，`journalctl -u pulse` 看得到服務為何停在 failed（例如憑證錯誤 4）
            service_status::attention(&failure.message.replace(['\r', '\n'], " "));
            failure.code
        }
    }
}

/// 下一個結束要求：結束訊號，或（Windows 服務）服務控制管理員的停止要求
async fn next_signal(
    signals: &mut Option<shutdown::Signals>,
    external: &CancellationToken,
) -> &'static str {
    let signal = async {
        match signals {
            Some(s) => {
                let name = s.recv().await;
                // 與服務的「停止要求」區分：日誌寫「收到 SIGTERM 訊號」
                match name {
                    "SIGINT" => "SIGINT 訊號",
                    "SIGTERM" => "SIGTERM 訊號",
                    "Ctrl+C" => "Ctrl+C 訊號",
                    "Ctrl+Break" => "Ctrl+Break 訊號",
                    "主控台關閉" => "主控台關閉訊號",
                    "系統關機" => "系統關機訊號",
                    other => other,
                }
            }
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        name = signal => name,
        _ = external.cancelled() => "服務控制管理員的停止要求",
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
/// 啟動失敗或背景任務異常結束時回傳 `Err`，程式以其中的結束代碼結束
/// （憑證與 `agent.id` 的問題為 [`EXIT_CERT`]，其餘為 [`EXIT_GENERAL`]）。
async fn run(
    config: AppConfig,
    retention: log_files::LogRetention,
    shutdown: CancellationToken,
) -> Result<(), Failure> {
    let mut config = config;
    // 完整版本字串（含 commit）；hello 的 agent_version 仍只送 CARGO_PKG_VERSION
    info!(
        "{}Pulse 地端程式啟動（{}，PID {}）...",
        codes::tag(codes::AGENT_START, None),
        cli::VERSION,
        std::process::id()
    );
    let cwd = std::env::current_dir().ok();
    // 本機狀態檔（pulse status）：之後各任務的狀態都記在這裡，每 10 秒與狀態變化時寫出
    let tracker = local_status::init(status_tracker(&config, cwd.as_deref()));
    for (level, msg) in startup_location_messages(cwd.as_deref(), config.local_file_found) {
        log::log!(level, "{msg}");
        if let Some(rest) = msg.strip_prefix(&codes::tag(
            codes::CONFIG_WARNING,
            Some(codes::W_SYSTEM32_WORKDIR),
        )) {
            tracker.update(|s| {
                s.add_config_warning(status_file::CodeNote::new(
                    codes::W_SYSTEM32_WORKDIR,
                    rest.to_string(),
                ));
            });
        }
    }
    info!(
        "本機狀態檔：{}（pulse status 讀取）",
        tracker.path().display()
    );
    tokio::spawn(local_status::run_writer(tracker.clone(), shutdown.clone()));
    // 本機指標（[metrics]；預設關閉）
    metrics::spawn(
        &config.metrics,
        cwd.as_deref().unwrap_or(Path::new(".")),
        shutdown.clone(),
    );
    debug!("使用的配置: {:?}", config);

    // TLS 設定只在啟動時建立一次；憑證 / 私鑰讀不到、格式錯誤、不成對、過期都會在這裡直接失敗。
    // 讀取系統信任庫與憑證檔是阻塞 I/O（系統信任庫可能很慢），放到 blocking 執行緒，
    // 且收到 Ctrl+C 時不等它完成
    // 註冊閘門（enroll::service）：沒有可用身分、而 enrollment.json 有進行中的申請時先完成申請；
    // 申請已結束（被拒、逾期…）時不連線，等待新的 pulse enroll
    let base = cwd.clone().unwrap_or_default();
    let paths = enroll::identity_paths(&config, &base);
    let mut background_pending = None;
    let mut start_in_grace = false;
    loop {
        match enroll::service::gate(&paths, &config, &base) {
            enroll::service::Gate::Normal { background } => {
                background_pending = background;
                break;
            }
            enroll::service::Gate::Grace(_) => {
                start_in_grace = true;
                break;
            }
            enroll::service::Gate::Pending(file) => {
                match enroll::service::run_pending(&paths, *file, &shutdown).await {
                    enroll::service::End::Shutdown => return Ok(()),
                    enroll::service::End::Installed => config = reload_config()?,
                    _ => {}
                }
            }
            enroll::service::Gate::Idle(file) => {
                if enroll::service::idle(&paths, &file, &shutdown).await
                    == enroll::service::End::Shutdown
                {
                    return Ok(());
                }
                config = reload_config()?;
            }
        }
    }

    let server_config = config.server.clone();
    let Some(tls) = blocking_or_shutdown(&shutdown, move || tls::build(&server_config))
        .await
        .map_err(|e| format!("啟動失敗：{e}"))?
    else {
        return Ok(());
    };
    let tls = tls.map_err(|e| Failure::cert(format!("啟動失敗：{e}")))?;
    if let Some(info) = tls.client_cert_info() {
        tracker.update(|s| s.set_client_cert(info));
    }

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
    let (agent_id, created) = loaded.map_err(|e| {
        let code = if e.kind() == std::io::ErrorKind::InvalidData {
            codes::E_AGENT_ID_INVALID
        } else {
            codes::E_AGENT_ID_IO
        };
        Failure::cert(format!("啟動失敗：{e}")).pls(code)
    })?;
    if let Some(warning) = permission_warning {
        warn!(
            "{}{warning}",
            codes::tag(codes::CONFIG_WARNING, Some(codes::W_AGENT_ID_PERMS))
        );
        tracker.update(|s| {
            s.add_identity_warning(status_file::CodeNote::new(codes::W_AGENT_ID_PERMS, warning));
        });
    }
    if created {
        info!(
            "{}已產生新的 agent_id {agent_id}，存放於 {}",
            codes::tag(codes::IDENTITY_CREATED, None),
            id_path.display()
        );
    } else {
        info!(
            "{}agent_id：{agent_id}（{}）",
            codes::tag(codes::IDENTITY_LOADED, None),
            id_path.display()
        );
    }
    // 客戶端憑證的 SAN 必須是 DNS:<agent_id>，否則伺服器會以 auth_failed 拒絕：啟動時就檢查
    tls.verify_client_cert_identity(&agent_id)
        .map_err(|e| Failure::cert(format!("啟動失敗：{e}")).pls(codes::E_CLIENT_CERT_MISMATCH))?;

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
    tracker.update(|s| s.set_identity(&agent_id, &hostname));
    // JSON 日誌的 agent_id：未啟用 mTLS 時 agent_id 等同身分憑據，與狀態檔相同不寫出
    if tls.client_cert_info().is_some()
        && let Some(sink) = log_files::sink()
    {
        sink.meta.set_agent_id(Some(agent_id.clone()));
    }

    spawn_client_cert_recheck(tls.clone());

    // 遠端管理（集中設定、遠端升級；crates/kairos-proto/REMOTE.md）：以快取的集中設定與本機設定合併出
    // 生效的設定；之後的即時變更由各工作就地套用，需要重新連線的變更重建取樣、本機緩衝與連線
    let mgmt = remote_setup(&config, cwd.as_deref());
    config = (*mgmt.config.effective()).clone();
    log::set_max_level(config.log.level_filter());
    spawn_live_logging(
        mgmt.config.subscribe(),
        retention,
        config.log.retention_days,
    );

    let mut tls = tls;
    // 狀態檔的「遠端管理」（pulse status 的「設定」「升級」列）
    {
        let mgmt = mgmt.clone();
        tracker.set_management(local_status::ManagementSource(Box::new(move || {
            status_file::Management {
                config: Some(mgmt.config.view()),
                upgrade: Some(mgmt.upgrade.view()),
            }
        })));
    }
    loop {
        let end = pipeline(
            &mut config,
            &mgmt,
            &agent_id,
            &hostname,
            &base,
            &paths,
            &mut tls,
            &tracker,
            &shutdown,
            background_pending.take(),
            std::mem::take(&mut start_in_grace),
        )
        .await?;
        match end {
            LoopEnd::Stopped => return Ok(()),
            LoopEnd::Reconfigure => {
                config = (*mgmt.config.effective()).clone();
                info!("以新的集中設定重建取樣、本機緩衝與連線");
            }
        }
    }
}

/// 建立遠端管理的狀態：集中設定（快取在狀態目錄的 `remote/config.json`）與遠端升級（安裝方式偵測、
/// `upgrade/state.json`）
fn remote_setup(config: &AppConfig, cwd: Option<&Path>) -> Arc<remote::RemoteMgmt> {
    let local_file = cwd
        .map(|d| d.join(config_manager::LOCAL_CONFIG_FILE))
        .filter(|p| p.is_file());
    let state_dir = remote::state_dir();
    let sources = remote::config::LocalSources::load(local_file.as_deref(), None);
    let central = remote::config::CentralConfig::new(
        config.clone(),
        sources,
        Some(state_dir.join(remote::config::CACHE_RELATIVE)),
    );
    // upgrade.enabled：pulse.toml 與環境變數都不為 false 才算開啟（輔助程式只讀 pulse.toml）
    let file_enabled = local_file
        .as_deref()
        .and_then(|p| AppConfig::load_file_only(p).ok())
        .is_none_or(|c| c.upgrade.enabled);
    let enabled = config.upgrade.enabled && file_enabled;
    let detection = remote::install::detect(
        enabled,
        &config.upgrade.allowed_signers_path(),
        &remote::install::Probe::system(),
    );
    // 每次連線前重新偵測（放好信任錨、啟用 pulse-upgrade.path 後不必重新啟動）。設定只在啟動時讀取，
    // 以 OnceLock 交給函式指標
    static DETECT_INPUT: std::sync::OnceLock<(bool, PathBuf)> = std::sync::OnceLock::new();
    let _ = DETECT_INPUT.set((enabled, config.upgrade.allowed_signers_path()));
    fn redetect() -> remote::install::Detection {
        let (enabled, anchor) = DETECT_INPUT.get().cloned().unwrap_or_default();
        remote::install::detect(enabled, &anchor, &remote::install::Probe::system())
    }
    info!(
        "遠端管理：集中設定{}；遠端升級 install.method={} install.upgrade={}",
        if config.remote_config.enabled {
            "啟用"
        } else {
            "本機停用"
        },
        detection.method,
        detection.upgrade
    );
    let agent = remote::upgrade::UpgradeAgent::new(remote::upgrade::UpgradeEnv {
        state_dir,
        detection,
        current_version: env!("CARGO_PKG_VERSION").to_string(),
        arch: std::env::consts::ARCH.to_string(),
        extra_principals: config.upgrade.extra_principals.clone(),
        allow_downgrade: config.upgrade.allow_downgrade,
        health_timeout_secs: config.upgrade.health_timeout_secs,
        require_root_owner: true,
        free_space: remote::upgrade::free_space,
        redetect: Some(redetect),
    });
    remote::RemoteMgmt::new(central, agent)
}

/// [`connection_loop`]／[`pipeline`] 結束的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopEnd {
    /// `shutdown` 被觸發
    Stopped,
    /// 集中設定改變了需要重新連線的鍵：以新的設定重建取樣、本機緩衝與連線
    Reconfigure,
}

/// 取樣（收集 task、本機緩衝、資產）與連線：以 `config`（生效的設定）建立，執行到結束要求或需要以新的
/// 集中設定重建為止。即時的鍵（`report.interval`、`collect.*`、`processes.*`、`inventory.*` 的間隔與
/// 選項、`buffer.max_*`）由這裡的工作訂閱生效的設定就地套用（REMOTE.md 第 3.1 節）
#[allow(clippy::too_many_arguments)]
async fn pipeline(
    config: &mut AppConfig,
    mgmt: &Arc<remote::RemoteMgmt>,
    agent_id: &str,
    hostname: &str,
    base: &Path,
    paths: &enroll::store::IdentityPaths,
    tls: &mut tls::TlsSetup,
    tracker: &Arc<local_status::Tracker>,
    shutdown: &CancellationToken,
    background_pending: Option<Box<enroll::store::EnrollmentFile>>,
    start_in_grace: bool,
) -> Result<LoopEnd, Failure> {
    // 重建時（集中設定改變了需要重新連線的鍵）：狀態檔不再顯示上一輪的本機緩衝
    tracker.clear_spool();
    // 本機緩衝（A5）：開啟失敗時不中止監控，改為 at-most-once（不宣告 durable）並記 ERROR
    let shared_spool = if config.buffer.enabled {
        let settings = spool_settings(&config.buffer);
        let dir = settings.dir.clone();
        match blocking_or_shutdown(shutdown, move || spool::Spool::open(settings))
            .await
            .map_err(|e| format!("啟動失敗：{e}"))?
        {
            None => return Ok(LoopEnd::Stopped),
            Some(Ok(opened)) => {
                let stats = opened.stats();
                info!(
                    "{}本機緩衝：{}（待送 {} 筆、{} bytes；上限 {} MiB／{} 筆，保留 {} 小時；補送間隔 {} 毫秒；\
                     本次 boot_id {}），Kairos 支援時 report 斷線期間的資料會補送（durable）",
                    codes::tag(codes::BUFFER_OPENED, None),
                    dir.display(),
                    stats.pending,
                    stats.pending_bytes,
                    config.buffer.max_mb,
                    config.buffer.max_records,
                    config.buffer.max_age_hours,
                    config.effective_replay_interval_ms(),
                    opened.boot_id()
                );
                if config.effective_replay_interval_ms() < config.buffer.replay_interval_ms {
                    info!(
                        "buffer.replay_interval_ms = {} 大於取樣間隔（report.interval = {} 秒）的 60%，\
                         改以 {} 毫秒送出（否則新的取樣比送出的快，積壓補不完）",
                        config.buffer.replay_interval_ms,
                        config.report.interval,
                        config.effective_replay_interval_ms()
                    );
                }
                let shared = spool::SharedSpool::new(opened);
                tracker.set_spool(shared.clone());
                Some(shared)
            }
            Some(Err(e)) => {
                let msg = format!(
                    "{}無法開啟本機緩衝 {}：{e}；本次執行不寫入本機緩衝，report 為 at-most-once（斷線期間的資料不補送）。\
                     請確認目錄可由服務帳號寫入（或以 buffer.dir 指定其他目錄），修正後重新啟動 Pulse",
                    codes::tag(codes::BUFFER_UNAVAILABLE, Some(codes::E_BUFFER_UNAVAILABLE)),
                    dir.display()
                );
                error!("{msg}");
                service_status::attention(&msg);
                tracker.update(|s| s.buffer_unavailable(e.to_string()));
                None
            }
        }
    } else {
        info!(
            "本機緩衝已停用（buffer.enabled = false）：report 為 at-most-once，斷線期間的資料不補送"
        );
        tracker.update(local_status::State::buffer_disabled);
        None
    };

    // 系統資訊收集 task →（啟用本機緩衝時經緩衝寫入 task）→ 連線 task。收集 task 依集中設定即時改變的
    // report.interval／collect.*／processes.* 重新開始取樣
    let (report_tx, mut report_rx) = mpsc::channel(REPORT_QUEUE_CAPACITY);
    let (mut collector, mut ingest) = match &shared_spool {
        Some(shared) => {
            let (sample_tx, sample_rx) = mpsc::channel(SPOOL_QUEUE_CAPACITY);
            let collector = tokio::spawn(collect_live(mgmt.config.subscribe(), sample_tx));
            let ingest = tokio::spawn(spool::ingest(sample_rx, shared.clone(), report_tx));
            (collector, Some(ingest))
        }
        None => (
            tokio::spawn(collect_live(mgmt.config.subscribe(), report_tx)),
            None,
        ),
    };
    // 本機緩衝的上限（buffer.max_mb／max_records／max_age_hours）即時生效
    let spool_limits = shared_spool.clone().map(|shared| {
        let mut live = mgmt.config.subscribe();
        tokio::spawn(async move {
            while live.changed().await.is_ok() {
                let s = spool_settings(&live.borrow_and_update().buffer);
                shared
                    .lock()
                    .set_limits(s.max_bytes, s.max_records, s.max_age);
            }
        })
    });
    let ingest_done = async {
        match ingest.as_mut() {
            Some(task) => task.await,
            None => std::future::pending().await,
        }
    };

    let mut settings = network::Settings::new(config, agent_id.to_string(), hostname.to_string());
    settings.mgmt = Some(mgmt.clone());
    if config.buffer.enabled && shared_spool.is_none() {
        // copywriting §4.2 範例 71：systemd Status 與服務描述也要看得到 PLS-E4004
        settings.status_note = Some("無法開啟本機緩衝（PLS-E4004，斷線期間的資料不會補送）".into());
    }
    // 主機資產（F4，feature inventory）：連線協商後才收集（背景的 blocking 執行緒）；間隔與選項即時生效
    let mut inventory_tasks = Vec::new();
    if config.inventory.enabled {
        let options = |c: &AppConfig| system_info::inventory::InventoryOptions {
            hardware_serials: c.inventory.hardware_serials,
            network_addresses: c.inventory.network_addresses,
            interface_exclude: c.collect.interface_exclude.clone(),
        };
        let agent = inventory::InventoryAgent::new(
            Duration::from_secs(config.inventory.interval),
            options(config),
        );
        inventory_tasks.push(tokio::spawn(inventory::run_worker(agent.clone())));
        let mut live = mgmt.config.subscribe();
        let live_agent = agent.clone();
        inventory_tasks.push(tokio::spawn(async move {
            while live.changed().await.is_ok() {
                let (interval, opts) = {
                    let c = live.borrow_and_update();
                    (Duration::from_secs(c.inventory.interval), options(&c))
                };
                live_agent.update(interval, opts);
            }
        }));
        settings.inventory = Some(agent);
    }
    if config.processes.enabled && config.processes.include_user {
        warn!(
            "{}processes.include_user = true：本版尚未支援送出行程的使用者名稱（METRICS.md 第 17 節 D3），忽略此設定",
            codes::tag(codes::CONFIG_WARNING, None)
        );
    }
    // 身分由 pulse enroll 管理（identity/client.pem）：宣告 renew、過期時改走 8444 過期續簽
    let managed = enroll::service::identity_managed(config, base, paths);
    if managed {
        settings.extra_features = vec![kairos_proto::FEATURE_RENEW.to_string()];
        settings.rotation = Some(Arc::new(Mutex::new(rotation::Rotation::new(
            paths.clone(),
            agent_id,
            config.identity.renew_before_days,
        ))));
        settings.grace_on_expiry = true;
    }
    let net = connection_loop(
        config,
        mgmt,
        base,
        paths,
        settings,
        tls,
        &mut report_rx,
        shared_spool.clone(),
        shutdown,
        background_pending,
        start_in_grace,
    );
    // (結果, 收集 task 已結束, 緩衝寫入 task 已結束)
    let (result, collector_done, ingest_finished) = tokio::select! {
        res = net => (res, false, false),
        res = ingest_done => (Err(match res {
            Ok(()) => "本機緩衝寫入任務意外結束".to_string(),
            Err(e) if e.is_panic() => format!("本機緩衝寫入任務發生 panic：{e}"),
            Err(e) => format!("本機緩衝寫入任務異常結束：{e}"),
        }
        .into()), false, true),
        // 收集 task 正常情況下不會結束；結束或 panic 時讓整個程式以非 0 結束，交由服務管理員重啟
        res = &mut collector => (Err(match res {
            Ok(()) => "系統資訊收集任務意外結束".to_string(),
            Err(e) if e.is_panic() => format!("系統資訊收集任務發生 panic：{e}"),
            Err(e) => format!("系統資訊收集任務異常結束：{e}"),
        }
        .into()), true, false),
    };
    // 重建或結束：停止這一輪的工作，並等它們真的停下來——重建時會以同一個目錄重新開啟本機緩衝
    // （緩衝不加鎖），舊的寫入 task 不能在新的緩衝開啟後還寫進同一個目錄
    collector.abort();
    if !collector_done {
        let _ = collector.await;
    }
    if let Some(task) = ingest {
        task.abort();
        if !ingest_finished {
            let _ = task.await;
        }
    }
    if let Some(task) = spool_limits {
        task.abort();
        let _ = task.await;
    }
    for task in inventory_tasks {
        task.abort();
        let _ = task.await;
    }
    // 結束（或異常結束、重建）前 fsync 本機緩衝並記下送達位置，下次開啟從這裡接著送
    if let Some(shared) = &shared_spool {
        let closed = shared.lock().close();
        if let Err(e) = closed {
            warn!("無法同步本機緩衝：{e}");
        }
    }
    result
}

/// 系統資訊收集 task：集中設定改變了取樣間隔或採集範圍（`report.interval`、`collect.*`、`processes.*`）時，
/// 以新的設定重新開始取樣（REMOTE.md 第 3.1 節「即時」；重新開始後的第一筆在一個間隔之後）
async fn collect_live(
    mut live: tokio::sync::watch::Receiver<Arc<AppConfig>>,
    tx: mpsc::Sender<system_info::Sample>,
) {
    loop {
        let (interval, opts) = {
            let c = live.borrow_and_update();
            (
                Duration::from_secs(c.report.interval),
                system_info::CollectOptions::from_config(&c),
            )
        };
        let collect = system_info::collect_periodically(interval, opts.clone(), tx.clone());
        tokio::pin!(collect);
        let restart = async {
            loop {
                if live.changed().await.is_err() {
                    return std::future::pending::<()>().await;
                }
                let c = live.borrow_and_update();
                if Duration::from_secs(c.report.interval) != interval
                    || system_info::CollectOptions::from_config(&c) != opts
                {
                    return;
                }
            }
        };
        tokio::select! {
            () = &mut collect => return,
            () = restart => {
                info!("取樣設定改變（集中設定），以新的設定重新開始取樣");
            }
        }
    }
}

/// 重新讀取設定（`pulse enroll` 寫入了 pulse.toml）並與集中設定合併；有誤時以設定錯誤結束（結束代碼 3）
fn reload_effective(mgmt: &remote::RemoteMgmt) -> Result<AppConfig, Failure> {
    let local = reload_config()?;
    let file = Path::new(config_manager::LOCAL_CONFIG_FILE);
    let sources = remote::config::LocalSources::load(file.is_file().then_some(file), None);
    Ok((*mgmt.config.replace_local(local, sources)).clone())
}

/// 重新讀取設定（`pulse enroll` 寫入了 pulse.toml）；有誤時以設定錯誤結束（結束代碼 3）
fn reload_config() -> Result<AppConfig, Failure> {
    AppConfig::load().map_err(|e| Failure {
        code: EXIT_CONFIG,
        message: format!("無法重新載入設定：{e}"),
        pls: Some(codes::E_CONFIG_SYNTAX),
    })
}

/// 依設定重新建立 TLS（取得新憑證後）
fn rebuild_tls(config: &AppConfig) -> Result<tls::TlsSetup, Failure> {
    let tls =
        tls::build(&config.server).map_err(|e| Failure::cert(format!("無法載入新憑證：{e}")))?;
    if let Some(info) = tls.client_cert_info() {
        local_status::record(|s| s.set_client_cert(info));
    }
    Ok(tls)
}

/// 連線、過期續簽與正常輪替的主迴圈（直到 `shutdown`）
#[allow(clippy::too_many_arguments)]
async fn connection_loop(
    config: &mut AppConfig,
    mgmt: &remote::RemoteMgmt,
    base: &Path,
    paths: &enroll::store::IdentityPaths,
    settings: network::Settings,
    tls: &mut tls::TlsSetup,
    reports: &mut mpsc::Receiver<system_info::Sample>,
    spool: Option<Arc<spool::SharedSpool>>,
    shutdown: &CancellationToken,
    background_pending: Option<Box<enroll::store::EnrollmentFile>>,
    start_in_grace: bool,
) -> Result<LoopEnd, Failure> {
    let mut grace_now = start_in_grace;
    let reload = Arc::new(tokio::sync::Notify::new());
    // 目前的身分仍可用、但有進行中的申請（pulse enroll --force 等）：在背景接續，取得後重新連線
    if let Some(file) = background_pending {
        let paths = paths.clone();
        let shutdown = shutdown.clone();
        let reload = reload.clone();
        tokio::spawn(async move {
            if enroll::service::run_pending(&paths, *file, &shutdown).await
                == enroll::service::End::Installed
            {
                reload.notify_one();
            }
        });
    }
    let info = enroll::flow::RequestInfo::new(&settings.hostname, &settings.host);
    loop {
        if shutdown.is_cancelled() {
            return Ok(LoopEnd::Stopped);
        }
        if grace_now {
            grace_now = false;
            let (s, t) = (settings.clone(), tls.clone());
            let end = enroll::service::run_grace(
                paths,
                config,
                base,
                &settings.agent_id,
                &info,
                shutdown,
                move || {
                    let (s, t) = (s.clone(), t.clone());
                    async move { network::check_in(&s, &t).await }
                },
            )
            .await;
            match end {
                enroll::service::End::Shutdown => return Ok(LoopEnd::Stopped),
                enroll::service::End::Installed => {
                    *tls = rebuild_tls(config)?;
                    info!(
                        "{}已透過過期續簽取得新憑證  via=grace",
                        codes::tag(codes::CERT_RENEWED, None)
                    );
                }
                enroll::service::End::NotExpired => {
                    // 依 Kairos 的時間仍有效：稍候以目前的憑證再連線
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                        _ = shutdown.cancelled() => return Ok(LoopEnd::Stopped),
                    }
                }
                enroll::service::End::Stopped | enroll::service::End::Changed => {
                    // 無法自動續簽：等待新的 pulse enroll（或服務重新啟動時再試一次）
                    let file = paths.load_enrollment().ok().flatten().unwrap_or_default();
                    if enroll::service::idle(paths, &file, shutdown).await
                        == enroll::service::End::Shutdown
                    {
                        return Ok(LoopEnd::Stopped);
                    }
                    *config = reload_effective(mgmt)?;
                    *tls = rebuild_tls(config)?;
                }
            }
            continue;
        }
        let stop = CancellationToken::new();
        let linker = {
            let stop = stop.clone();
            let shutdown = shutdown.clone();
            let reload = reload.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = shutdown.cancelled() => {}
                    _ = reload.notified() => {}
                }
                stop.cancel();
            })
        };
        let exit = network::run(settings.clone(), tls.clone(), reports, spool.clone(), stop).await;
        linker.abort();
        match exit {
            network::RunExit::Stopped if shutdown.is_cancelled() => return Ok(LoopEnd::Stopped),
            network::RunExit::Stopped => {
                // 背景申請取得了新憑證：重新讀設定並以新憑證連線
                *config = reload_effective(mgmt)?;
                *tls = rebuild_tls(config)?;
            }
            network::RunExit::Reconfigure => return Ok(LoopEnd::Reconfigure),
            network::RunExit::CertExpired => grace_now = true,
            network::RunExit::Renewed => {
                // 以新憑證另建連線送 hello，成功後才取代 client.pem／client.key（設計包 §5.6）
                let mut next = config.server.clone();
                next.client_cert = paths.next_cert().display().to_string();
                next.client_key = paths.next_key().display().to_string();
                let probe = match tls::build(&next) {
                    Ok(t) => network::one_shot(
                        &settings,
                        &t,
                        protocol::hello_features(protocol::OptionalFeatures::one_shot(
                            spool.is_some(),
                        )),
                        None,
                    )
                    .await
                    .map_err(|e| e.message),
                    Err(e) => Err(e),
                };
                match probe {
                    Ok(conn) => {
                        conn.close().await;
                        match rotation::promote(paths) {
                            Ok(serial) => {
                                info!(
                                    "{}已更新用戶端憑證（新序號 {serial}）  via=rotation",
                                    codes::tag(codes::CERT_RENEWED, None)
                                );
                                *tls = rebuild_tls(config)?;
                            }
                            Err(e) => warn!("無法以新憑證取代 client.pem：{e}；繼續使用目前的憑證"),
                        }
                    }
                    Err(e) => {
                        if let Some(rot) = &settings.rotation {
                            rot.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .probe_failed(&e);
                        }
                    }
                }
            }
        }
    }
}

/// `[buffer]` 設定對應的緩衝設定（已在載入時驗證範圍）
fn spool_settings(buffer: &config_manager::BufferConfig) -> spool::SpoolSettings {
    spool::SpoolSettings {
        dir: PathBuf::from(buffer.dir.trim()),
        max_bytes: buffer.max_mb.saturating_mul(1024 * 1024),
        max_records: usize::try_from(buffer.max_records).unwrap_or(usize::MAX),
        max_age: Duration::from_secs(buffer.max_age_hours.saturating_mul(3600)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_does_not_restart_when_retrying_cannot_help() {
        // pulse.service 的 RestartPreventExitStatus 必須恰好是用法、設定、憑證錯誤（2、3、4）；
        // 其他非 0 的結束代碼（一般錯誤 1、強制結束 130）仍由 Restart=always 重新啟動
        let unit = include_str!("../packaging/pulse.service");
        let line = unit
            .lines()
            .find(|l| l.starts_with("RestartPreventExitStatus="))
            .expect("pulse.service 應設定 RestartPreventExitStatus");
        let codes: Vec<&str> = line["RestartPreventExitStatus=".len()..]
            .split_whitespace()
            .collect();
        let expected = [EXIT_USAGE, EXIT_CONFIG, EXIT_CERT].map(|c| c.to_string());
        assert_eq!(codes, expected, "{line}");
        assert!(unit.lines().any(|l| l.trim() == "Restart=always"));
    }

    #[test]
    fn unit_lets_the_service_write_the_default_buffer_directory() {
        // A5：ProtectSystem=strict 下只有 ReadWritePaths= 可寫；預設的本機緩衝目錄必須在其中
        // （前綴 - 表示目錄不存在時不影響啟動，Pulse 改為 at-most-once 並記 ERROR）
        let unit = include_str!("../packaging/pulse.service");
        let line = unit
            .lines()
            .find(|l| l.starts_with("ReadWritePaths="))
            .expect("pulse.service 應設定 ReadWritePaths");
        let paths: Vec<&str> = line["ReadWritePaths=".len()..].split_whitespace().collect();
        let config = AppConfig::default_for_tests();
        let workdir = unit
            .lines()
            .find_map(|l| l.strip_prefix("WorkingDirectory="))
            .unwrap();
        assert_eq!(
            paths,
            [
                format!("{workdir}/logs"),
                format!("-{workdir}/{}", config.buffer.dir),
                format!("-{workdir}/{}", config.identity.dir)
            ],
            "{line}"
        );
    }

    #[test]
    fn unit_copies_the_system_serial_where_pulse_reads_it() {
        // METRICS.md D4：ExecStartPre=+（root、不受沙箱限制）把序號複製到 RuntimeDirectory，
        // 複製的目的地與 Pulse 讀取的位置相同，且失敗不影響啟動（前綴 -）
        let unit = include_str!("../packaging/pulse.service");
        let pre = unit
            .lines()
            .find_map(|l| l.strip_prefix("ExecStartPre="))
            .expect("pulse.service 應以 ExecStartPre= 複製序號");
        assert!(pre.starts_with("-+") || pre.starts_with("+-"), "{pre}");
        let args: Vec<&str> = pre.split_whitespace().collect();
        assert_eq!(args.last(), Some(&system_info::inventory::SERIAL_COPY_PATH));
        assert!(args.contains(&"/sys/class/dmi/id/product_serial"), "{pre}");
        assert!(args.windows(2).any(|w| w == ["-o", "pulse"]), "{pre}");
        let runtime = unit
            .lines()
            .find_map(|l| l.strip_prefix("RuntimeDirectory="))
            .expect("pulse.service 應設定 RuntimeDirectory");
        assert_eq!(
            Path::new(system_info::inventory::SERIAL_COPY_PATH).parent(),
            Some(Path::new("/run").join(runtime).as_path())
        );
        assert!(unit.lines().any(|l| l == "RuntimeDirectoryPreserve=yes"));
    }

    #[test]
    fn unit_lets_the_service_write_the_default_status_file() {
        // cli-spec §4.1：/var/lib/pulse/status.json 由 StateDirectory=pulse 建立，ProtectSystem=strict 下可寫
        let unit = include_str!("../packaging/pulse.service");
        let state_dir = unit
            .lines()
            .find_map(|l| l.strip_prefix("StateDirectory="))
            .expect("pulse.service 應設定 StateDirectory");
        assert_eq!(state_dir, "pulse");
        assert!(unit.lines().any(|l| l == "StateDirectoryMode=0755"));
        if cfg!(unix) {
            assert_eq!(
                status_file::default_path(),
                Path::new("/var/lib").join(state_dir).join("status.json")
            );
        }
        assert_eq!(AppConfig::default_for_tests().status.file, "");
    }

    #[test]
    fn failures_carry_exit_codes() {
        // cli-spec.md §1.3：未分類的錯誤為 1，憑證與 agent.id 的問題為 4
        let general: Failure = "系統資訊收集任務意外結束".to_string().into();
        assert_eq!(general.code, EXIT_GENERAL);
        assert_eq!(general.message, "系統資訊收集任務意外結束");
        let cert = Failure::cert("啟動失敗：找不到憑證".to_string());
        assert_eq!(cert.code, EXIT_CERT);
        assert_eq!(cert.message, "啟動失敗：找不到憑證");
        // 與設計包的代碼表一致
        assert_eq!(
            [EXIT_GENERAL, EXIT_USAGE, EXIT_CONFIG, EXIT_CERT],
            [1, 2, 3, 4]
        );
    }

    /// 文字中是否有「共 N 個」或「N 個測試」這類寫死的數量。
    fn has_hardcoded_count(line: &str) -> bool {
        let chars: Vec<char> = line.chars().collect();
        (0..chars.len()).any(|i| {
            let digits = chars[i..].iter().take_while(|c| c.is_ascii_digit()).count();
            if digits == 0 || (i > 0 && chars[i - 1].is_ascii_digit()) {
                return false;
            }
            let after: String = chars[i + digits..].iter().take(4).collect();
            let before: String = chars[i.saturating_sub(2)..i].iter().collect();
            after.starts_with(" 個") && (before == "共 " || after.starts_with(" 個測試"))
        })
    }

    #[test]
    fn readme_does_not_hardcode_test_count() {
        // Q8：測試數量隨時變動（且各平台不同），README 不寫死數字
        assert!(has_hardcoded_count("目前共 158 個。"));
        assert!(has_hardcoded_count("共有 12 個測試"));
        assert!(!has_hardcoded_count("目前共 2 次重試"));
        assert!(!has_hardcoded_count("憑證鏈 2 張"));
        let readme = include_str!("../README.md");
        let offending: Vec<&str> = readme
            .lines()
            .filter(|l| l.contains("測試") && has_hardcoded_count(l))
            .collect();
        assert!(offending.is_empty(), "{offending:?}");
    }

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
