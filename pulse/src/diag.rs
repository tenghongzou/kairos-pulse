//! `pulse check-config`（`pulse --check-config`）與 `pulse diag`（審計 O8；設計包 `pulse/cli-spec.md` §6、§8）。
//!
//! - `check-config`：載入設定（語法、範圍），再檢查伺服器、信任（`ca_cert`、`accept_invalid_certs`、
//!   `use_system_roots`）、用戶端憑證與私鑰（成對、到期、權限）、`agent.id`、日誌目錄與格式、syslog、
//!   `[metrics]` 與本機緩衝目錄。設定中的相對路徑與服務相同，以設定檔所在的目錄解析。不連線
//!   （`--connect` 尚未實作）。結束代碼：0 通過（有警告時仍為 0；`--strict` 時為 8）、3 設定錯誤、
//!   4 憑證或私鑰錯誤、2 用法錯誤。
//! - `diag`：產生支援診斷包（tar.gz，權限 0600；Windows 也是 tar.gz，可用內建的 `tar.exe` 解開）：
//!   `pulse status` 與 `check-config` 的輸出、設定（遮蔽後）、用戶端憑證的摘要（不含私鑰）、註冊進度的
//!   摘要（遮蔽 `poll_secret` 與 token）、最近的日誌、本機指標、系統與服務資訊、相關檔案清單（不含內容）。
//!   私鑰（`client.key`）、token、`poll_secret` 永不收集；`--redact strict` 另外把主機名稱與 IP 換成代號。

use chrono::{DateTime, Utc};
use kp_obs::bundle::{self, Bundle};
use kp_obs::check::{Check, Outcome};
use kp_obs::redact::Pseudonyms;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::{StatusArgs, UsageError};
use crate::codes;
use crate::config_manager::AppConfig;

/// 日誌收集的上限（`cli-spec.md` §8.5）
pub const MAX_LOG_BYTES: u64 = 50 * 1024 * 1024;
/// 預設收集的日誌期間
pub const DEFAULT_SINCE: Duration = Duration::from_secs(3 * 86_400);
/// 用戶端憑證在這麼多天內到期時警告
const CERT_WARN_DAYS: i64 = 14;

pub const CHECK_USAGE: &str = "\
pulse check-config — 檢查設定（不連線、不影響服務）

用法：
  pulse check-config [--strict] [--json] [--with-remote] [-c <pulse.toml>]
  pulse --check-config [...]               （相容寫法）

選項：
  -c, --config <路徑>  pulse.toml 的位置（預設依序找目前目錄、pulse 執行檔所在目錄）；
                       相對路徑與服務相同，以設定檔所在的目錄解析
      --strict         有警告時以結束代碼 8 結束
      --json           輸出 JSON（schema pulse.check-config/v1）
      --with-remote    連同集中設定的快取（狀態目錄的 remote/config.json）合併後再檢查一次

結束代碼：
  0 通過（有警告時仍為 0）  8 有警告（--strict）  3 設定錯誤  4 憑證或私鑰錯誤  2 用法錯誤";

pub const DIAG_USAGE: &str = "\
pulse diag — 產生支援診斷包（自動遮蔽私鑰、token 等敏感資料）

用法：
  pulse diag [--output <路徑>] [--redact <standard|strict>] [--since <期間>] [--json] [-c <pulse.toml>]

選項：
      --output <路徑>    輸出檔（預設為目前目錄的 pulse-diag-<主機名稱>-<UTC 時間>.tar.gz；已存在時不覆寫）
      --redact <層級>    standard（預設）：移除私鑰、token、poll_secret、密碼；
                         strict：另外把主機名稱與 IP 換成代號（對照表另存在本機，不放進診斷包）
      --since <期間>     收集多久的日誌，例如 24h、7d（預設 3d；合計上限 50 MB）
      --no-network       不做連線測試（目前的版本不做連線測試，可省略）
      --json             輸出 JSON（schema pulse.diag/v1）
  -c, --config <路徑>    pulse.toml 的位置

結束代碼：
  0 已產生  8 已產生，但有截斷或缺漏  10 權限不足（PLS-E8203）  1 無法寫出（PLS-E4301）  2 用法錯誤";

/// `check-config` 的參數
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CheckArgs {
    pub config: Option<String>,
    pub strict: bool,
    pub json: bool,
    /// 連同遠端設定的快取一起檢查（特權輔助程式的預檢；REMOTE.md 第 10.2 節）
    pub with_remote: bool,
}

/// `diag` 的參數
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagArgs {
    pub config: Option<String>,
    pub output: Option<PathBuf>,
    pub since: Duration,
    pub strict: bool,
    pub json: bool,
}

impl Default for DiagArgs {
    fn default() -> Self {
        Self {
            config: None,
            output: None,
            since: DEFAULT_SINCE,
            strict: false,
            json: false,
        }
    }
}

/// `24h`、`7d`、`30m`
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = num.parse().ok().filter(|n| *n > 0)?;
    let secs = match unit {
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return None,
    };
    Some(Duration::from_secs(n.checked_mul(secs)?))
}

fn value<'a>(
    args: &'a [String],
    i: &mut usize,
    name: &str,
    usage: &'static str,
) -> Result<&'a str, UsageError> {
    *i += 1;
    args.get(*i).map(String::as_str).ok_or_else(|| {
        UsageError::new(
            format!("{}：{name} 需要一個值", codes::E_BAD_ARG_VALUE),
            usage,
        )
    })
}

pub fn parse_check(args: &[String]) -> Result<CheckArgs, UsageError> {
    let mut out = CheckArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--strict" => out.strict = true,
            "--json" => out.json = true,
            "--with-remote" => out.with_remote = true,
            "-c" | "--config" => {
                out.config = Some(value(args, &mut i, "--config", CHECK_USAGE)?.to_string())
            }
            "--connect" | "--show-effective" => {
                return Err(UsageError::new(
                    format!(
                        "{}：{} 尚未支援（這個版本只檢查設定，不連線）",
                        codes::E_UNKNOWN_ARG,
                        args[i]
                    ),
                    CHECK_USAGE,
                ));
            }
            other => {
                return Err(UsageError::new(
                    format!(
                        "{}：pulse check-config 未知的參數「{other}」",
                        codes::E_UNKNOWN_ARG
                    ),
                    CHECK_USAGE,
                ));
            }
        }
        i += 1;
    }
    Ok(out)
}

pub fn parse_diag(args: &[String]) -> Result<DiagArgs, UsageError> {
    let mut out = DiagArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => out.json = true,
            "--no-network" => {}
            "-c" | "--config" => {
                out.config = Some(value(args, &mut i, "--config", DIAG_USAGE)?.to_string())
            }
            "-o" | "--output" => {
                out.output = Some(PathBuf::from(value(args, &mut i, "--output", DIAG_USAGE)?))
            }
            "--since" => {
                let v = value(args, &mut i, "--since", DIAG_USAGE)?;
                out.since = parse_duration(v).ok_or_else(|| {
                    UsageError::new(
                        format!(
                            "{}：--since 的期間無效（目前為「{v}」，例如 24h、7d）",
                            codes::E_BAD_ARG_VALUE
                        ),
                        DIAG_USAGE,
                    )
                })?;
            }
            "--redact" => match value(args, &mut i, "--redact", DIAG_USAGE)? {
                "standard" => out.strict = false,
                "strict" => out.strict = true,
                other => {
                    return Err(UsageError::new(
                        format!(
                            "{}：--redact 只接受 standard 或 strict（目前為「{other}」）",
                            codes::E_BAD_ARG_VALUE
                        ),
                        DIAG_USAGE,
                    ));
                }
            },
            other => {
                return Err(UsageError::new(
                    format!("{}：pulse diag 未知的參數「{other}」", codes::E_UNKNOWN_ARG),
                    DIAG_USAGE,
                ));
            }
        }
        i += 1;
    }
    Ok(out)
}

// ---- 設定檢查 ---------------------------------------------------------------------------------

/// 檢查的結果
#[derive(Debug)]
pub struct Report {
    pub config_file: Option<PathBuf>,
    pub base: PathBuf,
    pub checks: Vec<Check>,
    pub config: Option<AppConfig>,
    /// 路徑已以 `base` 解析成絕對路徑的設定（檔案的檢查與收集使用）
    pub resolved: Option<AppConfig>,
}

impl Report {
    pub fn render_text(&self) -> String {
        let header = format!(
            "{} · 設定：{}",
            crate::cli::VERSION,
            self.config_file.as_ref().map_or_else(
                || "（未找到 pulse.toml，使用內建預設值與環境變數）".to_string(),
                |p| p.display().to_string()
            )
        );
        kp_obs::check::render_text(&header, &self.checks, "Pulse")
    }

    pub fn to_json(&self, strict: bool) -> Value {
        kp_obs::check::to_json(
            "pulse.check-config/v1",
            &crate::status_file::ts(Utc::now()),
            &self.checks,
            strict,
            json!({"config_file": self.config_file.as_ref().map(|p| p.display().to_string())}),
        )
    }
}

/// 目錄是否可寫（建立一個暫存檔再刪除）；不存在時為 `None`
fn dir_writable(dir: &Path) -> Option<Result<(), String>> {
    if !dir.is_dir() {
        return None;
    }
    let probe = dir.join(format!(".pulse-check-{}.tmp", std::process::id()));
    Some(
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .map(|_| {
                let _ = std::fs::remove_file(&probe);
            })
            .map_err(|e| e.to_string()),
    )
}

/// 設定中的相對路徑以 `base`（設定檔所在的目錄，與服務的工作目錄相同）解析成絕對路徑
fn absolutize(c: &mut AppConfig, base: &Path) {
    let fix = |s: &mut String| {
        let t = s.trim();
        if !t.is_empty() && Path::new(t).is_relative() {
            *s = base.join(t).to_string_lossy().into_owned();
        }
    };
    fix(&mut c.server.ca_cert);
    fix(&mut c.server.client_cert);
    fix(&mut c.server.client_key);
    fix(&mut c.agent.id_file);
    fix(&mut c.identity.dir);
    fix(&mut c.buffer.dir);
    fix(&mut c.metrics.textfile);
}

/// 找設定檔：`--config` → 目前目錄 → 執行檔所在目錄（與 `pulse status` 相同）
fn locate(config: Option<&str>) -> Result<(Option<PathBuf>, PathBuf), Check> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let located = crate::status_cmd::locate(config.map(Path::new), &cwd, exe_dir.as_deref());
    if let Some(p) = &located.config
        && !p.is_file()
    {
        return Err(
            Check::new("syntax", "設定檔", Outcome::Error, "找不到指定的設定檔")
                .code(codes::E_CONFIG_NOT_FOUND)
                .detail(format!("原因：{} 不存在。", p.display())),
        );
    }
    if let Some(p) = &located.config
        && let Err(e) = std::fs::File::open(p)
    {
        return Err(Check::new("syntax", "設定檔", Outcome::Error, "無法讀取")
            .code(codes::E_CONFIG_UNREADABLE)
            .detail(format!("原因：{}：{e}", p.display()))
            .detail("處理方式：以 root（或服務帳號）執行。"));
    }
    Ok((located.config, located.base))
}

/// 執行所有檢查
pub fn check(config_arg: Option<&str>) -> Report {
    let (file, base) = match locate(config_arg) {
        Ok(v) => v,
        Err(c) => {
            return Report {
                config_file: config_arg.map(PathBuf::from),
                base: PathBuf::from("."),
                checks: vec![
                    c,
                    Check::new(
                        "others",
                        "其他檢查",
                        Outcome::Skipped,
                        "設定檔無法讀取，未繼續檢查",
                    ),
                ],
                config: None,
                resolved: None,
            };
        }
    };
    let mut checks = Vec::new();
    let loaded = match &file {
        Some(p) => AppConfig::load_file(p),
        None => AppConfig::load_file(&base.join(crate::config_manager::LOCAL_CONFIG_FILE)),
    };
    let config = match loaded {
        Ok(c) => {
            checks.push(Check::new(
                "syntax",
                "設定檔",
                Outcome::Ok,
                "合法的 TOML；所有欄位都在允許範圍內",
            ));
            c
        }
        Err(e) => {
            let code = match e {
                config::ConfigError::Message(_) => codes::E_CONFIG_RANGE,
                _ => codes::E_CONFIG_SYNTAX,
            };
            checks.push(
                Check::new("syntax", "設定檔", Outcome::Error, "無法載入")
                    .code(code)
                    .detail(format!("原因：{e}"))
                    .detail("處理方式：依訊息修正 pulse.toml 或 PULSE__ 環境變數，再執行 pulse check-config。"),
            );
            checks.push(Check::new(
                "others",
                "其他檢查",
                Outcome::Skipped,
                "設定檔無法解析，未繼續檢查",
            ));
            return Report {
                config_file: file,
                base,
                checks,
                config: None,
                resolved: None,
            };
        }
    };
    let mut resolved = config.clone();
    absolutize(&mut resolved, &base);
    let c = &config;
    let a = &resolved;

    let candidates = c.server.candidates();
    let fallback = if candidates.len() > 1 {
        let rest: Vec<String> = candidates[1..].iter().map(ToString::to_string).collect();
        format!("；備用位址：{}", rest.join("、"))
    } else {
        String::new()
    };
    checks.push(Check::new(
        "server",
        "伺服器",
        Outcome::Ok,
        format!(
            "{}（domain = {}）{fallback}",
            candidates[0],
            c.server.domain.trim()
        ),
    ));

    // 信任與用戶端憑證
    match crate::tls::build(&a.server) {
        Ok(tls) => {
            let mut trust = Check::new(
                "trust",
                "信任",
                Outcome::Ok,
                if c.server.accept_invalid_certs {
                    "不驗證伺服器憑證".to_string()
                } else if c.server.ca_cert.trim().is_empty() {
                    "作業系統信任庫".to_string()
                } else if c.server.trusts_system_roots() {
                    format!("{} 與作業系統信任庫", c.server.ca_cert.trim())
                } else {
                    format!("只信任 {}", c.server.ca_cert.trim())
                },
            );
            if c.server.accept_invalid_certs {
                trust = trust.code(codes::W_TLS_VERIFY_DISABLED);
                trust.raise(
                    Outcome::Warning,
                    "原因：server.accept_invalid_certs = true，任何人都能冒充 Kairos。處理方式：設定 server.ca_cert 並改回 false。",
                );
            } else if !c.server.ca_cert.trim().is_empty() && c.server.use_system_roots == Some(true)
            {
                trust = trust.code(codes::W_BOTH_TRUST_STORES);
                trust.raise(
                    Outcome::Warning,
                    "原因：設定了 server.ca_cert，但 server.use_system_roots 仍為 true；任何公開 CA 簽給同名的憑證也會被信任。處理方式：在 [server] 加上 use_system_roots = false。",
                );
            }
            checks.push(trust);
            match tls.client_cert_info() {
                Some(info) => {
                    let mut cert = Check::new(
                        "client_cert",
                        "用戶端憑證",
                        Outcome::Ok,
                        format!(
                            "{}；與私鑰成對{}",
                            info.path,
                            info.not_after.map_or(String::new(), |t| format!(
                                "；有效至 {}",
                                t.format("%Y-%m-%d")
                            ))
                        ),
                    );
                    if let Some(t) = info.not_after {
                        let left = (t - Utc::now()).num_days();
                        if t < Utc::now() {
                            cert = cert.code(codes::E_CLIENT_CERT_EXPIRED).cert_error();
                            cert.raise(Outcome::Error, "原因：用戶端憑證已過期；Kairos 會拒絕連線。處理方式：重新註冊（pulse enroll）或更換憑證。");
                        } else if left <= CERT_WARN_DAYS {
                            cert = cert.code(codes::W_CLIENT_CERT_EXPIRING);
                            cert.raise(
                                Outcome::Warning,
                                format!("原因：用戶端憑證將在 {left} 天內到期。"),
                            );
                        }
                    }
                    if let Some(w) = crate::file_perms::permission_warning(
                        Path::new(a.server.client_key.trim()),
                        crate::file_perms::CLIENT_KEY_FILE,
                    ) {
                        if cert.code.is_none() {
                            cert = cert.code(codes::W_KEY_PERMS);
                        }
                        cert.raise(Outcome::Warning, w);
                    }
                    checks.push(cert);
                }
                None => checks.push(Check::new(
                    "client_cert",
                    "用戶端憑證",
                    Outcome::Skipped,
                    "未設定（伺服器要求 mTLS 時需要；以 pulse enroll 註冊後會自動設定）",
                )),
            }
        }
        Err(e) => checks.push(
            Check::new("trust", "信任與憑證", Outcome::Error, "無法建立 TLS 設定")
                .code(codes::E_CLIENT_CERT_LOAD)
                .detail(format!("原因：{e}"))
                .cert_error(),
        ),
    }

    // agent.id
    let id_path = Path::new(a.agent.id_file.trim());
    let mut identity = Check::new("identity", "身分", Outcome::Ok, String::new());
    match std::fs::read_to_string(id_path) {
        Ok(s) => {
            let id = s.trim().trim_start_matches('\u{feff}').to_ascii_lowercase();
            if uuid::Uuid::parse_str(&id).is_ok() {
                identity.summary = format!("agent_id {id}");
            } else {
                identity = identity.code(codes::E_AGENT_ID_INVALID).cert_error();
                identity.summary = format!("{} 的內容不合法", id_path.display());
                identity.raise(
                    Outcome::Error,
                    "處理方式：刪除該檔案讓 Pulse 重新產生，或還原正確的 agent_id。",
                );
            }
            if let Some(w) = crate::agent_id::permission_warning(id_path) {
                if identity.code.is_none() {
                    identity = identity.code(codes::W_AGENT_ID_PERMS);
                }
                identity.raise(Outcome::Warning, w);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            identity.summary = format!("{} 尚不存在，第一次啟動時產生", id_path.display());
        }
        Err(e) => {
            identity = identity.code(codes::E_AGENT_ID_IO).cert_error();
            identity.summary = format!("無法讀取 {}", id_path.display());
            identity.raise(Outcome::Error, format!("原因：{e}"));
        }
    }
    checks.push(identity);

    // 日誌
    let logs = base.join(crate::log_files::LOG_DIR);
    let mut log = Check::new(
        "log",
        "日誌",
        Outcome::Ok,
        format!(
            "{} 可寫入；保留 {} 天；格式 {}{}",
            logs.display(),
            c.log.retention_days,
            c.log.format,
            if c.log.syslog.target.trim().is_empty() {
                String::new()
            } else {
                format!("；syslog {}", c.log.syslog.target.trim())
            }
        ),
    );
    match dir_writable(&logs) {
        Some(Err(e)) => log.raise(
            Outcome::Warning,
            format!("目前的帳號無法寫入日誌目錄（{e}）；服務帳號必須可寫。"),
        ),
        None => log.summary = format!("{}：不存在，啟動時建立", logs.display()),
        Some(Ok(())) => {}
    }
    if c.log.format.trim().eq_ignore_ascii_case("legacy") {
        log.raise(
            Outcome::Warning,
            "log.format = \"legacy\" 是 0.5 以前的格式（本地時間不帶時區），只保留一個主要版本供過渡；請改用 text 或 json。",
        );
        // 已棄用的設定值（設計包 INTEGRATION.md §28.5 #5；與 Kairos 的 KRS-W0104 同號）
        log = log.code("PLS-W0104");
    }
    checks.push(log);

    // 本機指標
    let m = &c.metrics;
    let mut metrics = Check::new(
        "metrics",
        "本機指標",
        Outcome::Ok,
        match (m.listen.trim(), m.textfile.trim()) {
            ("", "") => "未啟用".to_string(),
            (l, "") => format!("http://{l}/metrics"),
            ("", t) => format!("textfile {t}（每 {} 秒）", m.textfile_interval),
            (l, t) => format!("http://{l}/metrics；textfile {t}"),
        },
    );
    if !m.textfile.trim().is_empty() {
        let dir = Path::new(a.metrics.textfile.trim())
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        match dir_writable(dir) {
            None => metrics.raise(
                Outcome::Warning,
                format!("textfile 所在的目錄 {} 不存在（node_exporter 的 --collector.textfile.directory）。", dir.display()),
            ),
            Some(Err(e)) => metrics.raise(
                Outcome::Warning,
                format!("目前的帳號無法寫入 {}（{e}）；服務帳號必須可寫（systemd unit 的 ReadWritePaths）。", dir.display()),
            ),
            Some(Ok(())) => {}
        }
    }
    checks.push(metrics);

    // 本機緩衝
    if c.buffer.enabled {
        let dir = Path::new(a.buffer.dir.trim());
        let mut buf = Check::new(
            "buffer",
            "本機緩衝",
            Outcome::Ok,
            format!(
                "{}（上限 {} MiB／{} 筆）",
                dir.display(),
                c.buffer.max_mb,
                c.buffer.max_records
            ),
        );
        if let Some(Err(e)) = dir_writable(dir) {
            buf.raise(
                Outcome::Warning,
                format!("目前的帳號無法寫入緩衝目錄（{e}）；服務帳號必須可寫，否則改為 at-most-once（PLS-E4004）。"),
            );
        }
        checks.push(buf);
    } else {
        checks.push(Check::new(
            "buffer",
            "本機緩衝",
            Outcome::Skipped,
            "已停用（buffer.enabled = false）：斷線期間的資料不補送",
        ));
    }

    Report {
        config_file: file,
        base,
        checks,
        config: Some(config),
        resolved: Some(resolved),
    }
}

/// `pulse check-config`
pub fn run_check(args: CheckArgs) -> u8 {
    let mut report = check(args.config.as_deref());
    if args.with_remote {
        remote_check(&mut report);
    }
    if args.json {
        println!("{}", report.to_json(args.strict));
    } else {
        print!("{}", report.render_text());
    }
    kp_obs::check::exit_code(&report.checks, args.strict)
}

/// `--with-remote`：遠端設定快取與目前的本機設定合併後，是否仍通過設定檢查
fn remote_check(report: &mut Report) {
    let Some(local) = report.config.clone() else {
        return;
    };
    let sources = crate::remote::config::LocalSources::load(report.config_file.as_deref(), None);
    let cache = crate::remote::state_dir().join(crate::remote::config::CACHE_RELATIVE);
    let check = match crate::remote::config::effective_from_cache(&local, &sources, &cache) {
        Ok((_, None)) => Check::new(
            "remote_config",
            "集中設定",
            Outcome::Ok,
            if local.remote_config.enabled {
                "沒有下發的設定"
            } else {
                "本機停用（remote_config.enabled = false）"
            },
        ),
        Ok((m, Some(a))) if m.errors.is_empty() => Check::new(
            "remote_config",
            "集中設定",
            Outcome::Ok,
            format!(
                "第 {} 版與本機設定合併後合法（{} 個鍵由本機覆蓋）",
                a.rev,
                m.overridden.len()
            ),
        ),
        Ok((m, Some(a))) => Check::new(
            "remote_config",
            "集中設定",
            Outcome::Error,
            format!("第 {} 版與本機設定合併後不合法", a.rev),
        )
        .code(codes::W_REMOTE_CONFIG_REJECTED)
        .detail(format!(
            "原因：{}",
            m.errors
                .iter()
                .map(|e| format!("{}：{}", e.key, e.message))
                .collect::<Vec<_>>()
                .join("；")
        )),
        Err(e) => Check::new(
            "remote_config",
            "集中設定",
            Outcome::Warning,
            "遠端設定的快取無法使用（Pulse 會忽略它、改用本機設定）",
        )
        .code(codes::E_REMOTE_CONFIG_CACHE)
        .detail(format!("原因：{}：{e}", cache.display())),
    };
    report.checks.push(check);
}

// ---- 診斷包 -----------------------------------------------------------------------------------

const BUNDLE_ROOT: &str = "pulse-diag";

fn sha256_hex(data: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, data)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 註冊進度的摘要（`enrollment.json` 原檔不收集：只留下狀態類的欄位，秘密欄位由遮蔽移除）
fn enrollment_summary(path: &Path) -> Option<String> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let mut out = serde_json::Map::new();
    if let Value::Object(map) = v {
        for (k, val) in map {
            let lower = k.to_ascii_lowercase();
            if lower.contains("secret")
                || lower == "token"
                || lower.contains("nonce")
                || lower.contains("key")
            {
                out.insert(k, json!("[已移除]"));
            } else {
                out.insert(k, val);
            }
        }
    }
    serde_json::to_string_pretty(&Value::Object(out)).ok()
}

/// 每個候選位址（`server.address` 加 `server.fallback_addresses`）：名稱解析的結果與 TCP 連線
/// （每個位址最多 3 秒；只建立 TCP 連線，不做 TLS 握手、不送任何資料）
fn probe_candidates(c: &AppConfig) -> Vec<Value> {
    use std::net::ToSocketAddrs;
    c.server
        .candidates()
        .into_iter()
        .map(|t| {
            let resolved: Result<Vec<std::net::SocketAddr>, String> = (t.host.as_str(), t.port)
                .to_socket_addrs()
                .map(Iterator::collect)
                .map_err(|e| e.to_string());
            let (addresses, tcp) = match resolved {
                Ok(addrs) if addrs.is_empty() => (Vec::new(), "名稱解析沒有結果".to_string()),
                Ok(addrs) => {
                    let tcp = addrs
                        .iter()
                        .map(|a| std::net::TcpStream::connect_timeout(a, Duration::from_secs(3)))
                        .find_map(Result::ok)
                        .map_or_else(
                            || "無法連線（所有解析出的位址都失敗）".to_string(),
                            |_| "ok".to_string(),
                        );
                    (addrs.iter().map(|a| a.ip().to_string()).collect(), tcp)
                }
                Err(e) => (Vec::new(), format!("名稱解析失敗：{e}")),
            };
            json!({"server": t.to_string(), "addresses": addresses, "tcp": tcp})
        })
        .collect()
}

/// 本機指標：HTTP 或 textfile（都沒有時為 `None`）
fn metrics_snapshot(c: &AppConfig) -> Option<Result<String, String>> {
    if let Ok(Some(addr)) = c.metrics.listen_addr() {
        use std::io::{Read, Write};
        let r = (|| -> std::io::Result<String> {
            let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
            s.set_read_timeout(Some(Duration::from_secs(5)))?;
            s.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
            let mut buf = String::new();
            s.read_to_string(&mut buf)?;
            Ok(buf
                .split_once("\r\n\r\n")
                .map_or(String::new(), |(_, b)| b.to_string()))
        })();
        return Some(r.map_err(|e| format!("無法連線 http://{addr}/metrics：{e}")));
    }
    let t = c.metrics.textfile.trim();
    (!t.is_empty()).then(|| std::fs::read_to_string(t).map_err(|e| format!("無法讀取 {t}：{e}")))
}

/// 產生診斷包的內容（不寫檔；測試直接呼叫）
pub fn build(args: &DiagArgs, now: DateTime<Utc>) -> (Bundle, Report) {
    // pulse status 在切換目錄之前執行（它自己找設定檔）
    let status_json = crate::status_cmd::once(&StatusArgs {
        json: true,
        config: args.config.clone(),
        ..StatusArgs::default()
    });
    let status_text = crate::status_cmd::once(&StatusArgs {
        config: args.config.clone(),
        color: crate::cli::Color::Never,
        ..StatusArgs::default()
    });
    let report = check(args.config.as_deref());
    let host = sysinfo::System::host_name().unwrap_or_else(kp_obs::logfmt::hostname);
    let pseudonyms = args.strict.then(|| {
        let mut names = vec![host.clone()];
        if let Some(c) = &report.config {
            names.push(c.server.address.trim().to_string());
            // 備用位址（server.fallback_addresses）與主位址相同，換成代號
            names.extend(c.server.candidates().into_iter().skip(1).map(|t| t.host));
            names.push(c.server.domain.trim().to_string());
            names.push(c.enroll.address.trim().to_string());
            names.push(c.agent.display_name.trim().to_string());
        }
        Pseudonyms::new(names)
    });
    let mut b = Bundle::new(BUNDLE_ROOT, now.timestamp(), pseudonyms);

    // 狀態與設定檢查
    b.text("status.json", &status_json.stdout);
    let check_text = report.render_text();
    b.text(
        "check-config.json",
        &serde_json::to_string_pretty(&report.to_json(false)).unwrap_or_default(),
    );
    let check_result = match (
        kp_obs::check::count(&report.checks, Outcome::Error),
        kp_obs::check::count(&report.checks, Outcome::Warning),
    ) {
        (0, 0) => "ok",
        (0, _) => "warning",
        _ => "error",
    };
    b.section(
        "status",
        "ok",
        format!("狀態與設定（status 結束代碼 {}）", status_json.exit_code),
    );
    b.section(
        "check_config",
        check_result,
        kp_obs::check::summary_line(&report.checks, "Pulse"),
    );

    // 設定（遮蔽後）
    if let Some(f) = &report.config_file
        && let Ok(s) = std::fs::read_to_string(f)
    {
        b.text("config/pulse.toml", &s);
    }
    if let Some(c) = &report.config {
        b.text(
            "config/effective.json",
            &serde_json::to_string_pretty(c).unwrap_or_default(),
        );
    }
    let env: Vec<String> = std::env::vars()
        .filter(|(k, _)| k.starts_with("PULSE__") || k == "NO_COLOR" || k == "LANG")
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    b.text("config/environment.txt", &(env.join("\n") + "\n"));

    // 憑證與註冊（不含私鑰）
    if let Some(c) = &report.resolved {
        if let Ok(tls) = crate::tls::build(&c.server)
            && let Some(info) = tls.client_cert_info()
        {
            b.text(
                "cert.json",
                &serde_json::to_string_pretty(&json!({
                    "path": info.path,
                    "serial": info.serial,
                    "not_before": info.not_before.map(crate::status_file::ts),
                    "not_after": info.not_after.map(crate::status_file::ts),
                }))
                .unwrap_or_default(),
            );
        }
        let enrollment = Path::new(c.identity.dir.trim()).join("enrollment.json");
        if let Some(s) = enrollment_summary(&enrollment) {
            b.text("enrollment-summary.json", &s);
        }
        // 每個候選位址的名稱解析與 TCP 連線（kairos/HA.md 第 8.7 節）
        let probes = probe_candidates(c);
        let failed = probes.iter().filter(|p| p["tcp"] != "ok").count();
        b.text(
            "candidates.json",
            &serde_json::to_string_pretty(&probes).unwrap_or_default(),
        );
        b.section(
            "candidates",
            if failed == 0 { "ok" } else { "warning" },
            format!(
                "伺服器候選位址 {} 個，TCP 可連線 {} 個",
                probes.len(),
                probes.len() - failed
            ),
        );
        match metrics_snapshot(c) {
            Some(Ok(text)) => {
                b.text("metrics.prom", &text);
                b.section("metrics", "ok", "本機指標快照");
            }
            Some(Err(e)) => b.section("metrics", "warning", e),
            None => b.section("metrics", "skipped", "未啟用本機指標（[metrics]）"),
        }
    }

    // 日誌
    let files = bundle::log_files(
        &report.base.join(crate::log_files::LOG_DIR),
        crate::log_files::parse,
        args.since,
        now,
    );
    let (collected, total) = b.logs(&files, MAX_LOG_BYTES);
    let note = format!(
        "最近 {} 的日誌：{collected} 個檔案，{} KB{}",
        bundle::humanize(args.since),
        total / 1024,
        if b.truncated {
            format!(
                "；超過上限 {} MB，只收集較新的部分（{}）",
                MAX_LOG_BYTES / 1024 / 1024,
                codes::W_DIAG_TRUNCATED
            )
        } else {
            String::new()
        }
    );
    b.section("logs", if b.truncated { "warning" } else { "ok" }, note);

    // 系統與服務
    let mut sys = format!(
        "{}\n作業系統：{} {}\n核心：{}\n架構：{}\n本機時間：{}\nUTC：{}\n",
        crate::cli::VERSION,
        sysinfo::System::name().unwrap_or_default(),
        sysinfo::System::os_version().unwrap_or_default(),
        sysinfo::System::kernel_version().unwrap_or_default(),
        std::env::consts::ARCH,
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z"),
        crate::status_file::ts(now),
    );
    for f in ["/etc/resolv.conf", "/proc/loadavg"] {
        if let Ok(s) = std::fs::read_to_string(f) {
            sys.push_str(&format!("== {f}\n{s}\n"));
        }
    }
    b.text("system/os.txt", &sys);
    let t = Duration::from_secs(5);
    if cfg!(target_os = "linux") && Path::new("/run/systemd/system").exists() {
        b.text(
            "service/systemctl-cat.txt",
            &bundle::command_output("systemctl", &["cat", "pulse"], t),
        );
        b.text(
            "service/systemctl-show.txt",
            &bundle::command_output(
                "systemctl",
                &["show", "pulse", "-p", "ActiveState,SubState,MainPID,ExecMainStartTimestamp,NRestarts,User,WorkingDirectory,StandardOutput"],
                t,
            ),
        );
        b.text(
            "service/journal.txt",
            &bundle::command_output(
                "journalctl",
                &["-u", "pulse", "-n", "200", "--no-pager", "-o", "short-iso"],
                t,
            ),
        );
        b.section("service", "ok", "systemd 的設定與最近 200 行 journal");
    } else if cfg!(windows) {
        b.text(
            "service/sc-qc.txt",
            &bundle::command_output("sc.exe", &["qc", "Pulse"], t),
        );
        b.section("service", "ok", "Windows 服務設定（sc qc）");
    } else {
        b.section("service", "skipped", "不是以 systemd 執行");
    }

    // 檔案清單（不含內容）
    let mut paths = vec![report.base.join(crate::log_files::LOG_DIR)];
    if let Some(f) = &report.config_file {
        paths.push(f.clone());
    }
    if let Some(c) = &report.resolved {
        paths.push(PathBuf::from(c.agent.id_file.trim()));
        paths.push(PathBuf::from(c.identity.dir.trim()));
        paths.push(PathBuf::from(c.buffer.dir.trim()));
        for p in [
            &c.server.ca_cert,
            &c.server.client_cert,
            &c.server.client_key,
        ] {
            if !p.trim().is_empty() {
                paths.push(PathBuf::from(p.trim()));
            }
        }
    }
    b.text("files.txt", &bundle::file_listing(&paths));

    // 摘要（給支援人員第一眼看）
    let mut summary = format!(
        "{}\n產生時間：{}（UTC）\n\n",
        crate::cli::VERSION,
        crate::status_file::ts(now)
    );
    for s in &b.sections {
        summary.push_str(&format!("[{}] {}：{}\n", s.result, s.id, s.note));
    }
    summary.push_str("\n== pulse status\n");
    summary.push_str(&status_text.stdout);
    summary.push_str(&status_text.stderr);
    summary.push_str("\n== pulse check-config\n");
    summary.push_str(&check_text);
    b.text("summary.txt", &summary);
    (b, report)
}

/// 加上 MANIFEST.json 與 REDACTION.txt 並檢查沒有疑似秘密；回傳 tar.gz
pub fn finish(b: Bundle, args: &DiagArgs, now: DateTime<Utc>) -> Result<Vec<u8>, String> {
    b.finish(args.strict, |tar, b| {
        json!({
            "schema": "pulse.diag/v1",
            "generated_at": crate::status_file::ts(now),
            "version": env!("CARGO_PKG_VERSION"),
            "commit": env!("PULSE_BUILD_COMMIT"),
            "redaction": {
                "level": if args.strict { "strict" } else { "standard" },
                "removed": b.removed_json(),
            },
            "sections": b.sections.iter().map(bundle::Section::to_json).collect::<Vec<_>>(),
            "files": b.tar.files().iter().map(|(p, n)| json!({"path": p, "bytes": n})).collect::<Vec<_>>(),
            "tar_sha256": sha256_hex(tar),
        })
    })
}

/// `pulse diag`
pub fn run_diag(args: DiagArgs) -> u8 {
    let now = Utc::now();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let host = sysinfo::System::host_name().unwrap_or_else(kp_obs::logfmt::hostname);
    // 輸出位置以執行命令時的目錄解析（build 會切換到設定檔所在的目錄）
    let output = cwd.join(args.output.clone().unwrap_or_else(|| {
        PathBuf::from(format!(
            "pulse-diag-{}-{}.tar.gz",
            host.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "_"),
            now.format("%Y%m%dT%H%M%SZ")
        ))
    }));
    if !args.json {
        eprintln!("收集診斷資訊…");
    }
    let (bundle, report) = build(&args, now);
    if report
        .checks
        .iter()
        .any(|c| c.code == Some(codes::E_CONFIG_UNREADABLE))
    {
        let msg = "無法讀取設定檔；日誌與設定只有 root 與 pulse 群組可讀";
        if args.json {
            println!(
                "{}",
                json!({"schema": "pulse.diag/v1", "ok": false, "exit_code": 10,
                    "error": {"code": codes::E_NEED_ADMIN, "message": msg}})
            );
        } else {
            eprintln!(
                "✗ 錯誤 {}：需要系統管理員權限\n  原因：{msg}。\n  處理方式：以 root 執行：\n      sudo pulse diag",
                codes::E_NEED_ADMIN
            );
        }
        return 10;
    }
    let mapping = bundle
        .pseudonyms
        .as_ref()
        .map(|p| p.mapping().clone())
        .unwrap_or_default();
    let sections = bundle.sections.clone();
    let warnings = bundle.has_warnings();
    let truncated = bundle.truncated;
    let result = finish(bundle, &args, now).and_then(|data| {
        bundle::write_private(&output, &data)
            .map(|_| data)
            .map_err(|e| format!("無法寫入 {}：{e}", output.display()))
    });
    let data = match result {
        Ok(d) => d,
        Err(e) => {
            if args.json {
                println!(
                    "{}",
                    json!({"schema": "pulse.diag/v1", "ok": false, "exit_code": 1,
                        "error": {"code": codes::E_DIAG_FAILED, "message": e}})
                );
            } else {
                eprintln!(
                    "✗ 錯誤 {}：無法寫出診斷包\n  原因：{e}",
                    codes::E_DIAG_FAILED
                );
            }
            return 1;
        }
    };
    let mut mapping_file = None;
    if !mapping.is_empty() {
        let p = output.with_extension("mapping.txt");
        let text: String = mapping.iter().map(|(k, v)| format!("{v}\t{k}\n")).collect();
        if bundle::write_private(&p, text.as_bytes()).is_ok() {
            mapping_file = Some(p);
        }
    }
    let code = if warnings { 8 } else { 0 };
    if args.json {
        println!(
            "{}",
            json!({
                "schema": "pulse.diag/v1",
                "ok": true,
                "generated_at": crate::status_file::ts(now),
                "exit_code": code,
                "data": {
                    "output": output.display().to_string(),
                    "size_bytes": data.len(),
                    "sha256": sha256_hex(&data),
                    "redaction": {"level": if args.strict { "strict" } else { "standard" },
                        "mapping_file": mapping_file.as_ref().map(|p| p.display().to_string())},
                    "sections": sections.iter().map(bundle::Section::to_json).collect::<Vec<_>>(),
                },
                "warnings": [],
                "error": null,
            })
        );
        return code;
    }
    for s in &sections {
        let mark = match s.result {
            "ok" => "✓",
            "skipped" => "-",
            "error" => "✗",
            _ => "⚠",
        };
        println!("{mark} {}：{}", s.id, s.note);
    }
    println!(
        "\n✓ 已產生 {}（{} KB，權限 0600）\n  SHA-256：{}",
        output.display(),
        data.len().div_ceil(1024),
        sha256_hex(&data)
    );
    match &mapping_file {
        Some(p) => println!(
            "  遮蔽層級：strict（主機名稱與 IP 已換成代號；對照表 {}，不在診斷包中）",
            p.display()
        ),
        None => println!(
            "  遮蔽層級：standard（保留主機名稱與 IP；若需要隱藏，請改用 --redact strict）"
        ),
    }
    println!("  下一步：把這個檔案交給支援人員。檔案內容可以先解開檢查（summary.txt 是摘要）。");
    if truncated {
        println!(
            "\n⚠ 警告 {}：日誌超過大小上限，已截斷\n  原因：要求收集的日誌超過 {} MB，只收集了較新的部分。\n  處理方式：以 --since 指定較短的期間，或直接提供 logs/ 中特定日期的檔案。",
            codes::W_DIAG_TRUNCATED,
            MAX_LOG_BYTES / 1024 / 1024
        );
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn candidates_are_probed_with_tcp_only() {
        // 一個在監聽、一個已關閉（Windows 對已關閉的 loopback 埠約 2 秒後才回報拒絕，仍在 3 秒內）
        let open = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_port = closed.local_addr().unwrap().port();
        drop(closed);
        let mut c = AppConfig::default_for_tests();
        c.server.address = "127.0.0.1".into();
        c.server.port = open.local_addr().unwrap().port();
        c.server.fallback_addresses = vec![format!("127.0.0.1:{closed_port}")];
        let probes = probe_candidates(&c);
        assert_eq!(probes.len(), 2, "{probes:?}");
        assert_eq!(probes[0]["tcp"], "ok", "{probes:?}");
        assert_eq!(probes[0]["addresses"], json!(["127.0.0.1"]));
        assert_ne!(probes[1]["tcp"], "ok", "{probes:?}");
        assert_eq!(probes[1]["server"], format!("127.0.0.1:{closed_port}"));
    }

    #[test]
    fn args_and_durations() {
        assert_eq!(parse_duration("24h"), Some(Duration::from_secs(86_400)));
        assert_eq!(parse_duration("7d"), Some(Duration::from_secs(7 * 86_400)));
        assert_eq!(parse_duration("0d"), None);
        assert_eq!(parse_duration("3x"), None);
        assert_eq!(parse_duration("d"), None);
        let d = parse_diag(&s(&[
            "--redact", "strict", "--since", "30m", "-o", "x.tar.gz",
        ]))
        .unwrap();
        assert!(d.strict);
        assert_eq!(d.since, Duration::from_secs(1800));
        assert!(parse_diag(&s(&["--redact", "none"])).is_err());
        assert!(parse_diag(&s(&["--bogus"])).is_err());
        let c = parse_check(&s(&["--strict", "--json", "-c", "p.toml"])).unwrap();
        assert!(c.strict && c.json);
        assert_eq!(c.config.as_deref(), Some("p.toml"));
        assert!(parse_check(&s(&["--connect"])).is_err());
    }

    #[test]
    fn legacy_log_format_is_warned_with_a_code() {
        let dir = std::env::temp_dir().join(format!("pulse-diag-l-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("pulse.toml");
        std::fs::write(
            &cfg,
            "[log]
format = \"legacy\"
",
        )
        .unwrap();
        let r = check(Some(&cfg.to_string_lossy()));
        let text = r.render_text();
        assert!(text.contains("PLS-W0104"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diag_bundle_contains_no_secrets() {
        let dir = std::env::temp_dir().join(format!("pulse-diag-b-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::create_dir_all(dir.join("identity")).unwrap();
        let cfg = dir.join("pulse.toml");
        std::fs::write(
            &cfg,
            "[server]\naddress = \"kairos.example.gov.tw\"\ndomain = \"kairos.example.gov.tw\"\n# KRT1-7G4Q-M2XD-9KQP-A1B2\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("identity/client.key"),
            "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49SECRETKEY\n-----END PRIVATE KEY-----\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("identity/enrollment.json"),
            r#"{"state":"pending","poll_secret":"0123456789abcdef0123456789abcdef","token":"KRT1-7G4Q-M2XD-9KQP-A1B2"}"#,
        )
        .unwrap();
        let today = Utc::now().format("%Y-%m-%d");
        std::fs::write(
            dir.join(format!("logs/pulse-{today}.log")),
            "2026-10-14T06:00:00.000Z [INFO] 測試 Authorization: Bearer abcdefSECRET pulse-enroll:v1:eyJ0b2tlbiI6IngifQ\n",
        )
        .unwrap();
        let args = DiagArgs {
            config: Some(cfg.to_string_lossy().into_owned()),
            ..DiagArgs::default()
        };
        let now = Utc::now();
        let (b, report) = build(&args, now);
        assert!(report.config.is_some(), "{}", report.render_text());
        let gz = finish(b, &args, now).expect("應可產生");
        let tar = kp_obs::archive::gunzip(&gz).unwrap();
        let text = String::from_utf8_lossy(&tar);
        assert!(kp_obs::redact::find_leak(&text).is_none());
        for secret in [
            "SECRETKEY",
            "0123456789abcdef0123456789abcdef",
            "M2XD-9KQP",
            "abcdefSECRET",
            "eyJ0b2tlbiI6IngifQ",
        ] {
            assert!(!text.contains(secret), "診斷包中不應有 {secret}");
        }
        assert!(text.contains("pulse-diag/MANIFEST.json"));
        assert!(text.contains("pulse-diag/logs/pulse-"));
        assert!(text.contains("client.key"), "檔案清單列出名稱（不含內容）");
        // strict：主機名稱換成代號
        let args = DiagArgs {
            strict: true,
            ..args
        };
        let (b, _) = build(&args, now);
        let tar = kp_obs::archive::gunzip(&finish(b, &args, now).unwrap()).unwrap();
        assert!(!String::from_utf8_lossy(&tar).contains("kairos.example.gov.tw"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enrollment_summary_removes_secrets() {
        let dir = std::env::temp_dir().join(format!("pulse-diag-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("enrollment.json");
        std::fs::write(
            &p,
            r#"{"state":"pending","request_code":"7KQ2-M9XD","poll_secret":"deadbeefdeadbeef","token":"KRT1-7G4Q-M2XD-9KQP-A1B2","token_prefix":"KRT1-7G4Q"}"#,
        )
        .unwrap();
        let out = enrollment_summary(&p).unwrap();
        assert!(
            !out.contains("deadbeef") && !out.contains("M2XD-9KQP"),
            "{out}"
        );
        assert!(out.contains("7KQ2-M9XD") && out.contains("\"token_prefix\": \"KRT1-7G4Q\""));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
