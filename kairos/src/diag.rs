//! `kairos --check-config` 與 `kairos diag`（審計 O8；設計包 `design-system/cli-style.md` §5.5、§8、§10、
//! `kairos/screens.md` S20「下載診斷包」）。
//!
//! - `--check-config`：載入設定（語法、型別、範圍、矛盾），再檢查監聽位址、TLS 憑證與到期、主控台憑證、
//!   資料與日誌目錄、日誌與 syslog 設定、`[observability]`、檔案描述元與容量估算。不啟動服務、不連線。
//!   結束代碼：0 通過（有警告時仍為 0，加 `--strict` 時為 8）；3 設定錯誤；4 憑證錯誤。
//! - `diag`：產生支援診斷包（tar.gz，權限 0600）：版本、設定（遮蔽 token、密碼、私鑰）、`--check-config`
//!   的結果、最近的日誌、資料庫與狀態表摘要、執行中服務的 `/readyz` 與 `/metrics` 快照、systemd 的設定與
//!   journal、相關檔案清單（不含內容）。私鑰檔的內容一律不收集；產生後再以 [`kp_obs::redact::find_leak`]
//!   檢查一次，仍有疑似秘密時不寫出。

use chrono::{DateTime, Utc};
use kp_obs::bundle::{self, Bundle};
use kp_obs::redact::Pseudonyms;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{AppConfig, DEFAULT_CONFIG_FILE, StorageBackend};

/// 錯誤代碼：設定檔語法或型別錯誤（設計包 `decisions/INTEGRATION.md` §28.3；類比 `PLS-E0101`）
pub const CODE_CONFIG_LOAD: &str = "KRS-E0101";
/// `--config` 指定的設定檔不存在（§28.3；類比 `PLS-E0401`）
pub const CODE_CONFIG_MISSING: &str = "KRS-E0401";
/// 設定檔無法讀取：權限、I/O（§28.3；類比 `PLS-E0402`）
pub const CODE_CONFIG_UNREADABLE: &str = "KRS-E0402";
/// 設定值超出範圍
pub const CODE_CONFIG_RANGE: &str = "KRS-E0201";
/// 無法產生匯出檔案（`copywriting.md` §3.3 4301）
pub const CODE_DIAG_FAILED: &str = "KRS-E4301";
/// 診斷包內容已截斷（§28.3；類比 `PLS-W4302`）
pub const CODE_DIAG_TRUNCATED: &str = "KRS-W4302";

/// 日誌收集的上限（與 `pulse diag` 相同，`cli-spec.md` §8.5）
pub const MAX_LOG_BYTES: u64 = 50 * 1024 * 1024;
/// 預設收集的日誌期間
pub const DEFAULT_SINCE: Duration = Duration::from_secs(3 * 86_400);
/// 連線到執行中的服務的逾時
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

pub const CHECK_USAGE: &str = "\
kairos --check-config — 檢查設定後結束（不啟動服務、不連線）

用法：
  kairos --check-config [--config <path>] [--strict] [--json]
  kairos check-config [...]                    （同上）

選項：
  --config <path>  設定檔（預設為目前目錄的 kairos.toml）
  --strict         有警告時以結束代碼 8 結束
  --json           輸出 JSON（schema kairos.check-config/v1）

結束代碼：
  0 通過（有警告時仍為 0）  8 有警告（--strict）  3 設定錯誤  4 憑證錯誤  2 用法錯誤";

pub const DIAG_USAGE: &str = "\
kairos diag — 產生支援診斷包（tar.gz，權限 0600）

用法：
  kairos diag [--config <path>] [--output <檔案>] [--since <期間>] [--redact-hosts] [--no-server] [--json]

選項：
  --config <path>   設定檔（預設為目前目錄的 kairos.toml）
  --output <檔案>   輸出檔（預設為目前目錄的 kairos-diag-<主機名稱>-<UTC 時間>.tar.gz；已存在時不覆寫）
  --since <期間>    收集多久的日誌，例如 24h、7d（預設 3d；合計上限 50 MB，超過時只收集較新的）
  --redact-hosts    把主機名稱與 IP 換成一致的代號（交給外部廠商時使用；對照表另存，不放進診斷包）
  --no-server       不連線到執行中的 Kairos（不收集 /readyz 與 /metrics）
  --json            輸出 JSON（schema kairos.diag/v1）

一律遮蔽：私鑰、主控台 API token、註冊 token、安裝字串、Bearer、設定中名稱含 token／secret／password 的值。
私鑰檔的內容永不收集。

結束代碼：
  0 已產生  8 已產生，但有截斷、缺漏或警告  9 已產生，設定檢查有錯誤  1 無法產生（KRS-E4301）  2 用法錯誤";

/// `--check-config` 的參數
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CheckArgs {
    pub config_path: Option<PathBuf>,
    pub strict: bool,
    pub json: bool,
}

/// `kairos diag` 的參數
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagArgs {
    pub config_path: Option<PathBuf>,
    pub output: Option<PathBuf>,
    pub since: Duration,
    pub redact_hosts: bool,
    pub no_server: bool,
    pub json: bool,
}

impl Default for DiagArgs {
    fn default() -> Self {
        Self {
            config_path: None,
            output: None,
            since: DEFAULT_SINCE,
            redact_hosts: false,
            no_server: false,
            json: false,
        }
    }
}

fn utf8(a: &OsString) -> Result<&str, String> {
    a.to_str().ok_or_else(|| {
        format!(
            "無法辨識的參數（不是有效的 Unicode）: {}",
            a.to_string_lossy()
        )
    })
}

pub fn parse_check_args(args: impl IntoIterator<Item = OsString>) -> Result<CheckArgs, String> {
    let mut out = CheckArgs::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if let Some(p) = crate::config::take_config_arg(&a, &mut it)? {
            if out.config_path.replace(p).is_some() {
                return Err("--config 只能指定一次".into());
            }
            continue;
        }
        match utf8(&a)? {
            "--strict" => out.strict = true,
            "--json" => out.json = true,
            other => return Err(format!("無法辨識的參數: {other}")),
        }
    }
    Ok(out)
}

pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<DiagArgs, String> {
    let mut out = DiagArgs::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if let Some(p) = crate::config::take_config_arg(&a, &mut it)? {
            if out.config_path.replace(p).is_some() {
                return Err("--config 只能指定一次".into());
            }
            continue;
        }
        match utf8(&a)? {
            "--output" | "-o" => {
                let v = it.next().ok_or("--output 需要指定檔案")?;
                out.output = Some(PathBuf::from(v));
            }
            "--since" => {
                let v = it.next().ok_or("--since 需要指定期間，例如 24h、7d")?;
                let v = utf8(&v)?.to_string();
                out.since = crate::hosts_cli::parse_duration(&v)
                    .filter(|d| !d.is_zero())
                    .ok_or_else(|| format!("--since 的期間無效：{v}（例如 24h、7d）"))?;
            }
            "--redact-hosts" => out.redact_hosts = true,
            "--no-server" => out.no_server = true,
            "--json" => out.json = true,
            "-h" | "--help" => return Err(DIAG_USAGE.to_string()),
            other => return Err(format!("無法辨識的參數: {other}")),
        }
    }
    Ok(out)
}

// ---- 設定檢查 ---------------------------------------------------------------------------------

pub use kp_obs::check::{Check, Outcome};

/// 檢查的結果
#[derive(Debug, Clone)]
pub struct CheckReport {
    pub config_file: Option<PathBuf>,
    pub checks: Vec<Check>,
    /// 載入成功時的設定
    pub config: Option<AppConfig>,
}

impl CheckReport {
    pub fn count(&self, o: Outcome) -> usize {
        kp_obs::check::count(&self.checks, o)
    }

    /// 結束代碼（`cli-style.md` §8）
    pub fn exit_code(&self, strict: bool) -> u8 {
        kp_obs::check::exit_code(&self.checks, strict)
    }

    /// 「結果：1 個錯誤、2 個警告、14 項通過。」
    pub fn summary_line(&self) -> String {
        kp_obs::check::summary_line(&self.checks, "Kairos")
    }

    /// 人看的輸出
    pub fn render_text(&self) -> String {
        let header = format!(
            "{} · 設定：{}",
            crate::VERSION,
            self.config_file.as_ref().map_or(
                "（未找到，使用程式內預設值）".to_string(),
                |p| p.display().to_string()
            )
        );
        kp_obs::check::render_text(&header, &self.checks, "Kairos")
    }

    pub fn to_json(&self, strict: bool) -> Value {
        kp_obs::check::to_json(
            "kairos.check-config/v1",
            &crate::storage::fmt_time(Utc::now()),
            &self.checks,
            strict,
            json!({"config_file": self.config_file.as_ref().map(|p| p.display().to_string())}),
        )
    }
}

/// 實際讀取的設定檔（指定的，或目前目錄的 kairos.toml；都沒有時為 `None`）
fn config_file(path: Option<&Path>) -> Option<PathBuf> {
    match path {
        Some(p) => Some(p.to_path_buf()),
        None => {
            let d = Path::new(DEFAULT_CONFIG_FILE);
            d.is_file().then(|| d.to_path_buf())
        }
    }
}

/// [`AppConfig::load`] 失敗的產品代碼（`--check-config`、啟動與 CLI 共用；設計包 §28.3、§28.5 #1）：
/// 值超出範圍 `KRS-E0201`、互相矛盾 `KRS-E0202`、`--config` 指定的檔案不存在 `KRS-E0401`、
/// 無法讀取 `KRS-E0402`、其餘（語法、型別）`KRS-E0101`
pub fn config_load_code(e: &std::io::Error, path: Option<&Path>) -> &'static str {
    if let Some(c) = kp_obs::logfmt::find_code(&e.to_string()) {
        if c == "KRS-E0202" {
            "KRS-E0202"
        } else {
            CODE_CONFIG_RANGE
        }
    } else if e.kind() == ErrorKind::InvalidInput {
        CODE_CONFIG_RANGE
    } else if e.kind() == ErrorKind::NotFound {
        CODE_CONFIG_MISSING
    } else if config_file(path).is_some_and(|f| std::fs::read(f).is_err()) {
        CODE_CONFIG_UNREADABLE
    } else {
        CODE_CONFIG_LOAD
    }
}

/// 目錄是否可寫（建立一個暫存檔再刪除）；不存在時為 `None`
fn dir_writable(dir: &Path) -> Option<Result<(), String>> {
    if !dir.is_dir() {
        return None;
    }
    let probe = dir.join(format!(".kairos-check-{}.tmp", std::process::id()));
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

/// 解析監聽位址（主機名稱以系統的解析器）
fn resolve(addr: &str) -> Result<Vec<std::net::SocketAddr>, String> {
    use std::net::ToSocketAddrs;
    addr.trim()
        .to_socket_addrs()
        .map(|a| a.collect())
        .map_err(|e| e.to_string())
}

/// 執行所有檢查
pub fn check(path: Option<&Path>) -> CheckReport {
    let file = config_file(path);
    let mut checks = Vec::new();
    let config = match AppConfig::load(path) {
        Ok(c) => {
            checks.push(Check::new(
                "config",
                "設定檔",
                Outcome::Ok,
                match &file {
                    Some(f) => format!("{} 可以載入，所有值都在允許範圍內", f.display()),
                    None => "未找到 kairos.toml，使用程式內預設值與 KAIROS__ 環境變數".to_string(),
                },
            ));
            c
        }
        Err(e) => {
            let msg = e.to_string();
            let code = config_load_code(&e, path);
            checks.push(
                Check::new("config", "設定檔", Outcome::Error, "無法載入")
                    .code(code)
                    .detail(format!("原因：{msg}"))
                    .detail("處理方式：依訊息修正設定檔或 KAIROS__ 環境變數後，再執行 kairos --check-config。"),
            );
            checks.push(Check::new(
                "others",
                "其他檢查",
                Outcome::Skipped,
                "設定檔無法載入，未繼續檢查",
            ));
            return CheckReport {
                config_file: file,
                checks,
                config: None,
            };
        }
    };
    let c = &config;

    // 監聽位址與 S1
    let mut listen = Check::new("listen", "監聽位址", Outcome::Ok, String::new());
    match resolve(&c.server.listen) {
        Ok(addrs) => match c.check_listen_auth(&addrs) {
            Ok(auth) => {
                listen.summary = format!("agent {}", c.server.listen);
                if matches!(
                    auth,
                    crate::config::ListenAuth::UnauthenticatedAllowed { .. }
                ) {
                    listen.result = Outcome::Warning;
                    listen = listen.code("KRS-W0302").detail(
                        "原因：未啟用 mTLS 卻監聽非 loopback 位址（auth.allow_unauthenticated = true），任何能連線的人都能以任意 agent_id 註冊。",
                    ).detail("處理方式：正式環境請設定 auth.client_ca 啟用 mTLS。");
                }
            }
            Err(e) => {
                listen.result = Outcome::Error;
                listen.summary = format!("agent {}", c.server.listen);
                listen = listen.code("KRS-E0301").detail(format!("原因：{e}"));
            }
        },
        Err(e) => {
            listen.result = Outcome::Error;
            listen.summary = format!("無法解析 server.listen {}", c.server.listen);
            listen = listen.code(CODE_CONFIG_RANGE).detail(format!("原因：{e}"));
        }
    }
    let mut others = Vec::new();
    if c.console.enabled {
        others.push(format!("主控台 {}", c.console.listen));
    }
    if c.enroll_possible() {
        others.push(format!(
            "註冊端點 {}（{}）",
            c.enroll.listen_addr(&c.server.listen),
            c.enroll.effective_mode().as_str()
        ));
    }
    if !c.observability.listen.trim().is_empty() {
        others.push(format!("探針 http://{}", c.observability.listen.trim()));
        if let Err(e) = resolve(&c.observability.listen) {
            listen.result = listen.result.max(Outcome::Error);
            listen = listen
                .code(CODE_CONFIG_RANGE)
                .detail(format!("原因：無法解析 observability.listen：{e}"));
        }
    }
    if !others.is_empty() {
        listen.summary = format!("{} · {}", listen.summary, others.join(" · "));
    }
    checks.push(listen);

    // agent 連線埠的 TLS
    let pki_files = (c.storage.backend == StorageBackend::Sqlite
        && c.auth.client_ca_path().is_some())
    .then(|| {
        let d = Path::new(&c.storage.data_dir);
        (
            crate::pki::issuing_cas_path(d)
                .to_string_lossy()
                .into_owned(),
            crate::pki::issuing_crl_path(d)
                .to_string_lossy()
                .into_owned(),
        )
    })
    .filter(|(a, _)| Path::new(a).is_file());
    let paths = crate::tls::TlsPaths {
        cert: c.server.cert_path.clone(),
        key: c.server.key_path.clone(),
        client_ca: c.auth.client_ca_path().map(str::to_string),
        crls: c.auth.crl_paths(),
        issuing_cas: pki_files.as_ref().map(|(a, _)| a.clone()),
        issuing_crl: pki_files
            .as_ref()
            .map(|(_, b)| b.clone())
            .filter(|p| Path::new(p).is_file()),
        retired_cas: (c.storage.backend == StorageBackend::Sqlite
            && c.auth.client_ca_path().is_some())
        .then(|| {
            crate::pki::retired_cas_path(Path::new(&c.storage.data_dir))
                .to_string_lossy()
                .into_owned()
        })
        .filter(|p| Path::new(p).is_file()),
    };
    match crate::tls::build(&paths) {
        Ok(material) => {
            let now = Utc::now();
            let mut tls = Check::new(
                "tls",
                "TLS 憑證",
                Outcome::Ok,
                format!(
                    "{}{}",
                    c.server.cert_path,
                    if material.auth_enabled() {
                        format!("；mTLS，CRL {} 份", material.crl_count)
                    } else {
                        "；未啟用 mTLS".to_string()
                    }
                ),
            );
            for e in &material.expiries {
                let days = (e.at - now).num_seconds() as f64 / 86_400.0;
                let (what, warn_days, code) = match e.kind {
                    crate::expiry::ExpiryKind::ServerCert => ("伺服器憑證", 30.0, "KRS-W1001"),
                    crate::expiry::ExpiryKind::Crl => ("CRL", 7.0, "KRS-W1201"),
                };
                if days <= warn_days {
                    tls.result = Outcome::Warning;
                    tls.code = Some(code);
                    tls.details.push(format!(
                        "{what} {} {}（{}）",
                        e.path,
                        if days < 0.0 {
                            "已過期".to_string()
                        } else {
                            format!("{:.0} 天後到期", days.floor())
                        },
                        crate::storage::fmt_time(e.at)
                    ));
                }
            }
            if let Some(w) = crate::fsperm::key_permission_warning(Path::new(&c.server.key_path)) {
                tls.result = tls.result.max(Outcome::Warning);
                tls.details.push(w);
            }
            checks.push(tls);
        }
        Err(e) => {
            let mut chk = Check::new("tls", "TLS 憑證", Outcome::Error, "無法建立 TLS 設定")
                .detail(format!("原因：{e}"))
                .detail("處理方式：確認 server.cert_path、server.key_path、auth.client_ca、auth.crl 的檔案存在、可讀且成對。");
            chk.cert = true;
            checks.push(chk);
        }
    }

    // 主控台
    if !c.console.enabled {
        checks.push(Check::new(
            "console",
            "主控台",
            Outcome::Skipped,
            "已停用（console.enabled = false）",
        ));
    } else if c.storage.backend == StorageBackend::Jsonl {
        checks.push(
            Check::new(
                "console",
                "主控台",
                Outcome::Warning,
                "storage.backend = \"jsonl\" 時不啟動",
            )
            .detail("主控台需要 SQLite；不需要主控台時可設定 console.enabled = false。"),
        );
    } else {
        let (cert, key) = c.console.tls_files(&c.server);
        match crate::api::build_acceptor(&cert, &key) {
            Ok(_) => checks.push(Check::new(
                "console",
                "主控台",
                Outcome::Ok,
                format!("https://{}/（憑證 {cert}）", c.console.listen),
            )),
            Err(e) => {
                let mut chk =
                    Check::new("console", "主控台", Outcome::Error, "主控台的憑證無法使用")
                        .detail(format!("原因：{e}"));
                chk.cert = true;
                checks.push(chk);
            }
        }
    }

    // 資料目錄
    let data = Path::new(&c.storage.data_dir);
    let mut storage = Check::new(
        "storage",
        "資料目錄",
        Outcome::Ok,
        format!(
            "{}（{}）",
            data.display(),
            match c.storage.backend {
                StorageBackend::Sqlite => "SQLite",
                StorageBackend::Jsonl => "JSONL",
            }
        ),
    );
    match dir_writable(data) {
        None => {
            storage.summary.push_str("；不存在，啟動時建立");
        }
        Some(Ok(())) => {}
        Some(Err(e)) => {
            storage.result = Outcome::Warning;
            storage.details.push(format!(
                "目前的帳號無法寫入 {}（{e}）；服務帳號（例如 kairos）必須可寫。",
                data.display()
            ));
        }
    }
    if let Some(w) = crate::fsperm::dir_permission_warning(data, "資料目錄") {
        storage.result = storage.result.max(Outcome::Warning);
        storage.details.push(w);
    }
    if let Some((max, estimate)) = c.total_limit_below_estimate() {
        storage.result = storage.result.max(Outcome::Warning);
        storage.details.push(format!(
            "storage.max_total_bytes（{max} bytes）小於以 server.max_connections 台主機估算的容量（約 {estimate} bytes）；達到上限後會提早刪除最舊的資料。"
        ));
    }
    checks.push(storage);

    // 日誌
    let logs = Path::new(crate::logger::LOG_DIR);
    let mut log = Check::new(
        "log",
        "日誌",
        Outcome::Ok,
        format!(
            "{}（相對於工作目錄）；格式 {}；保留 {} 天{}",
            logs.display(),
            c.log.format,
            c.log.retention_days,
            if c.log.syslog.target.trim().is_empty() {
                String::new()
            } else {
                format!("；syslog {}", c.log.syslog.target.trim())
            }
        ),
    );
    if let Some(Err(e)) = dir_writable(logs) {
        log.result = Outcome::Warning;
        log.details.push(format!(
            "目前的帳號無法寫入日誌目錄（{e}）；服務帳號必須可寫。"
        ));
    }
    if c.log.format.trim().eq_ignore_ascii_case("legacy") {
        log.result = log.result.max(Outcome::Warning);
        log = log.code("KRS-W0104").detail(
            "log.format = \"legacy\" 是 0.5 以前的格式（本地時間不帶時區），只保留一個主要版本供過渡；請改用 text 或 json。",
        );
    }
    checks.push(log);

    // 健康檢查與 Prometheus
    let o = &c.observability;
    let mut obs = Check::new(
        "observability",
        "健康檢查",
        Outcome::Ok,
        format!(
            "{}{}；/metrics {}",
            if o.on_console && c.console.enabled {
                "主控台的監聽"
            } else {
                "主控台不提供"
            },
            if o.listen.trim().is_empty() {
                String::new()
            } else {
                format!("＋http://{}", o.listen.trim())
            },
            if o.metrics_token_file.trim().is_empty() {
                "只接受 loopback 來源"
            } else {
                "需要 bearer token"
            }
        ),
    );
    if !o.metrics_token_file.trim().is_empty() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(m) = std::fs::metadata(o.metrics_token_file.trim())
                && m.permissions().mode() & 0o007 != 0
            {
                obs.result = Outcome::Warning;
                obs.details.push(format!(
                    "{} 其他使用者可讀（{:04o}）；建議 chmod 640 並只讓服務帳號的群組讀取。",
                    o.metrics_token_file.trim(),
                    m.permissions().mode() & 0o7777
                ));
            }
        }
    }
    if (!o.on_console || !c.console.enabled) && o.listen.trim().is_empty() {
        obs.result = obs.result.max(Outcome::Warning);
        obs.details.push(
            "/healthz、/readyz、/metrics 目前沒有任何監聽位址提供；設定 observability.listen（例如 127.0.0.1:9464）。"
                .to_string(),
        );
    }
    checks.push(obs);

    // 允許清單（KRS-W0203）與檔案描述元
    if let Some(w) = c.allowlisted_limit_warning() {
        checks.push(
            Check::new(
                "allowlist",
                "每 IP 上限",
                Outcome::Warning,
                "允許清單內的上限被限縮",
            )
            .code("KRS-W0203")
            .detail(w),
        );
    }
    let needed = c.fd_needed();
    let mut fd = Check::new(
        "fd",
        "檔案描述元",
        Outcome::Ok,
        format!(
            "需要 {needed}（建議 LimitNOFILE={}）",
            crate::config::suggested_nofile(needed)
        ),
    );
    if let Some(soft) = crate::nofile_soft_limit()
        && soft < needed
    {
        fd.result = Outcome::Warning;
        fd.details.push(format!(
            "目前這個 shell 的上限為 {soft}；以 systemd 執行時以 unit 的 LimitNOFILE 為準（發佈包預設 8192）。"
        ));
    }
    checks.push(fd);

    CheckReport {
        config_file: file,
        checks,
        config: Some(config),
    }
}

/// `kairos --check-config`
pub fn run_check(args: CheckArgs) -> u8 {
    let report = check(args.config_path.as_deref());
    if args.json {
        println!("{}", report.to_json(args.strict));
    } else {
        print!("{}", report.render_text());
    }
    report.exit_code(args.strict)
}

// ---- 診斷包 -----------------------------------------------------------------------------------

/// tar 中的頂層目錄
const BUNDLE_ROOT: &str = "kairos-diag";

/// 以 HTTP/1.1 取得執行中服務的一個端點：(狀態碼, 本文)
async fn fetch(config: &AppConfig, path: &str) -> Result<(u16, String), String> {
    let token = config
        .observability
        .metrics_token()
        .ok()
        .flatten()
        .filter(|_| path == "/metrics");
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nUser-Agent: kairos-diag\r\n{auth}Connection: close\r\n\r\n"
    );
    let loopback = |addr: &str| -> Result<std::net::SocketAddr, String> {
        let a = resolve(addr)?
            .into_iter()
            .next()
            .ok_or_else(|| format!("無法解析 {addr}"))?;
        Ok(match a.ip() {
            std::net::IpAddr::V4(v) if v.is_unspecified() => {
                std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), a.port())
            }
            std::net::IpAddr::V6(v) if v.is_unspecified() => {
                std::net::SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), a.port())
            }
            _ => a,
        })
    };
    let raw = if !config.observability.listen.trim().is_empty() {
        let addr = loopback(&config.observability.listen)?;
        let mut s = tokio::time::timeout(FETCH_TIMEOUT, tokio::net::TcpStream::connect(addr))
            .await
            .map_err(|_| format!("連線 {addr} 逾時"))?
            .map_err(|e| format!("無法連線 {addr}：{e}"))?;
        read_response(&mut s, &request).await?
    } else if config.console.enabled && config.observability.on_console {
        let addr = loopback(&config.console.listen)?;
        let (cert, _) = config.console.tls_files(&config.server);
        let tls = pinned_connector(&cert)?;
        let tcp = tokio::time::timeout(FETCH_TIMEOUT, tokio::net::TcpStream::connect(addr))
            .await
            .map_err(|_| format!("連線 {addr} 逾時"))?
            .map_err(|e| format!("無法連線 {addr}：{e}"))?;
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost")
            .map_err(|e| e.to_string())?;
        let mut s = tokio::time::timeout(FETCH_TIMEOUT, tls.connect(name, tcp))
            .await
            .map_err(|_| "TLS 握手逾時".to_string())?
            .map_err(|e| format!("TLS 握手失敗：{e}"))?;
        read_response(&mut s, &request).await?
    } else {
        return Err("沒有提供 /healthz、/readyz、/metrics 的監聽（observability.listen 未設定且主控台未提供）".into());
    };
    let (head, body) = raw.split_once("\r\n\r\n").ok_or("回應格式不正確")?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("回應格式不正確")?;
    Ok((status, body.to_string()))
}

async fn read_response<S>(s: &mut S, request: &str) -> Result<String, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    tokio::time::timeout(FETCH_TIMEOUT, async move {
        s.write_all(request.as_bytes()).await?;
        s.flush().await?;
        let mut buf = Vec::new();
        s.take(16 * 1024 * 1024).read_to_end(&mut buf).await?;
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&buf).into_owned())
    })
    .await
    .map_err(|_| "讀取回應逾時".to_string())?
    .map_err(|e| format!("讀取回應失敗：{e}"))
}

/// 只信任主控台自己的憑證（以 DER 完全相同比對；不依賴 CA 與名稱）的 TLS 連線器
fn pinned_connector(cert_file: &str) -> Result<tokio_rustls::TlsConnector, String> {
    use tokio_rustls::rustls;
    use tokio_rustls::rustls::pki_types::pem::PemObject;
    let pinned = rustls::pki_types::CertificateDer::pem_file_iter(cert_file)
        .map_err(|e| format!("無法讀取主控台的憑證 {cert_file}：{e}"))?
        .next()
        .ok_or_else(|| format!("{cert_file} 中沒有憑證"))?
        .map_err(|e| format!("無法讀取主控台的憑證 {cert_file}：{e}"))?
        .into_owned();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(Pinned {
        cert: pinned.as_ref().to_vec(),
        provider: provider.clone(),
    });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

use std::sync::Arc;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};

#[derive(Debug)]
struct Pinned {
    cert: Vec<u8>,
    provider: Arc<tokio_rustls::rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
        _server_name: &tokio_rustls::rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        if end_entity.as_ref() == self.cert.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(tokio_rustls::rustls::Error::General(
                "伺服器憑證與設定的主控台憑證不同".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        tokio_rustls::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        tokio_rustls::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// 資料庫的離線摘要（唯讀開啟）
fn database_summary(config: &AppConfig) -> Result<Value, String> {
    let path = crate::store::sqlite::SqliteOptions::from_config(
        config,
        Path::new(&config.storage.data_dir),
    )
    .path;
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("無法開啟 {}：{e}", path.display()))?;
    let _ = conn.busy_timeout(Duration::from_secs(2));
    let stats = crate::api::health::read_db_stats(&conn).map_err(|e| e.to_string())?;
    let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).ok();
    Ok(json!({
        "path": path.display().to_string(),
        "used_bytes": stats.used_bytes,
        "page_size": stats.page_size,
        "max_total_bytes": config.storage.max_total_bytes,
        "schema_version": conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).ok(),
        "hosts": count("SELECT COUNT(*) FROM hosts"),
        "hosts_online": count("SELECT COUNT(*) FROM hosts WHERE online = 1"),
        "capacity_event": stats.capacity.as_ref().map(|c| json!({
            "active": c.ended_at.is_none(),
            "started_at": crate::api::http::ts_ms(c.started_at),
            "ended_at": c.ended_at.map(crate::api::http::ts_ms),
        })),
        "retention_last_run": stats.retention.as_ref().map(|r| json!({
            "finished_at": crate::api::http::ts_ms(r.finished_at),
            "deleted_raw": r.deleted_raw,
        })),
        "retention_paused": stats.guard.as_ref().is_some_and(|g| g.jump_since_unix.is_some() || g.rollback_since_unix.is_some()),
    }))
}

/// 產生診斷包的內容（不寫檔；測試直接呼叫）
pub fn build(args: &DiagArgs, now: DateTime<Utc>) -> (Bundle, CheckReport) {
    let report = check(args.config_path.as_deref());
    let host = kp_obs::logfmt::hostname();
    let pseudonyms = args.redact_hosts.then(|| {
        let mut names = vec![host.clone()];
        if let Some(c) = &report.config {
            for addr in [&c.server.listen, &c.console.listen, &c.observability.listen] {
                if let Some((h, _)) = addr.trim().rsplit_once(':') {
                    names.push(h.trim_matches(['[', ']']).to_string());
                }
            }
            if let Some((h, _)) = c.enroll.advertise.trim().rsplit_once(':') {
                names.push(h.trim_matches(['[', ']']).to_string());
            }
        }
        Pseudonyms::new(names)
    });
    let mut b = Bundle::new(BUNDLE_ROOT, now.timestamp(), pseudonyms);

    // 版本與設定檢查
    b.text("version.txt", &format!("{}\n", crate::VERSION));
    let check_text = report.render_text();
    b.text("check-config.txt", &check_text);
    b.text(
        "check-config.json",
        &serde_json::to_string_pretty(&report.to_json(false)).unwrap_or_default(),
    );
    let check_result = match (report.count(Outcome::Error), report.count(Outcome::Warning)) {
        (0, 0) => "ok",
        (0, _) => "warning",
        _ => "error",
    };
    b.section("check_config", check_result, report.summary_line());

    // 設定
    if let Some(file) = &report.config_file {
        match std::fs::read_to_string(file) {
            Ok(s) => b.text("config/kairos.toml", &s),
            Err(e) => b.section(
                "config_file",
                "warning",
                format!("無法讀取 {}：{e}", file.display()),
            ),
        }
    }
    if let Some(c) = &report.config {
        b.text(
            "config/effective.json",
            &serde_json::to_string_pretty(c).unwrap_or_default(),
        );
    }
    let env: Vec<String> = std::env::vars()
        .filter(|(k, _)| k.starts_with("KAIROS__") || k == "LANG" || k == "TZ" || k == "NO_COLOR")
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    b.text("config/environment.txt", &(env.join("\n") + "\n"));
    b.section(
        "config",
        "ok",
        "設定檔（遮蔽後）、實際生效的設定、KAIROS__ 環境變數",
    );

    // 日誌
    let files = bundle::log_files(
        Path::new(crate::logger::LOG_DIR),
        |n| {
            crate::logger::LOG_FILES
                .parse(n)
                .or_else(|| crate::logger::LOG_FILES_JSON.parse(n))
        },
        args.since,
        now,
    );
    let (collected, total) = b.logs(&files, MAX_LOG_BYTES);
    b.section(
        "logs",
        if b.truncated { "warning" } else { "ok" },
        format!(
            "最近 {} 的日誌：{collected} 個檔案，{} KB{}",
            bundle::humanize(args.since),
            total / 1024,
            if b.truncated {
                format!(
                    "；超過上限 {} MB，只收集較新的部分（{CODE_DIAG_TRUNCATED}）",
                    MAX_LOG_BYTES / 1024 / 1024
                )
            } else {
                String::new()
            }
        ),
    );

    // 資料庫與狀態表
    if let Some(c) = &report.config {
        if c.storage.backend == StorageBackend::Sqlite {
            match database_summary(c) {
                Ok(v) => {
                    b.text(
                        "status/database.json",
                        &serde_json::to_string_pretty(&v).unwrap_or_default(),
                    );
                    b.section("database", "ok", "資料庫的用量、主機數與保留清除的狀態");
                }
                Err(e) => b.section("database", "warning", e),
            }
        }
        let agents = Path::new(&c.storage.data_dir).join("agents.json");
        if let Ok(s) = std::fs::read_to_string(&agents)
            && let Ok(v) = serde_json::from_str::<Value>(&s)
        {
            let list = v.get("agents").and_then(Value::as_array);
            let online = list.map_or(0, |a| a.iter().filter(|x| x["online"] == true).count());
            b.text(
                "status/agents-summary.json",
                &json!({"agents": list.map_or(0, Vec::len), "online": online, "generated_at": v.get("generated_at")}).to_string(),
            );
        }
    }

    // 執行中服務的狀態
    if let Some(c) = report.config.as_ref().filter(|_| !args.no_server) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        match rt {
            Ok(rt) => {
                let (ready, metrics) =
                    rt.block_on(async { (fetch(c, "/readyz").await, fetch(c, "/metrics").await) });
                match ready {
                    Ok((code, body)) => {
                        b.text("status/readyz.json", &body);
                        b.section(
                            "server",
                            if code == 200 { "ok" } else { "warning" },
                            format!("/readyz 回 {code}"),
                        );
                    }
                    Err(e) => b.section(
                        "server",
                        "warning",
                        format!("無法連線到執行中的 Kairos：{e}"),
                    ),
                }
                match metrics {
                    Ok((200, body)) => {
                        b.text("status/metrics.prom", &body);
                        b.section("metrics", "ok", "/metrics 快照");
                    }
                    Ok((code, _)) => b.section("metrics", "warning", format!("/metrics 回 {code}")),
                    Err(e) => b.section("metrics", "warning", e),
                }
            }
            Err(e) => b.section("server", "warning", format!("無法建立 runtime：{e}")),
        }
    } else if args.no_server {
        b.section("server", "skipped", "--no-server：未連線到執行中的 Kairos");
    }

    // 系統與服務
    let mut sys = String::new();
    for f in [
        "/etc/os-release",
        "/proc/version",
        "/proc/loadavg",
        "/proc/meminfo",
    ] {
        if let Ok(s) = std::fs::read_to_string(f) {
            let s: String = s.lines().take(20).collect::<Vec<_>>().join("\n");
            sys.push_str(&format!("== {f}\n{s}\n"));
        }
    }
    sys.push_str(&format!(
        "== 時間\n本機 {} · UTC {}\n",
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z"),
        crate::storage::fmt_time(now)
    ));
    b.text("system/os.txt", &sys);
    if cfg!(target_os = "linux") && Path::new("/run/systemd/system").exists() {
        let t = Duration::from_secs(5);
        b.text(
            "service/systemctl-cat.txt",
            &bundle::command_output("systemctl", &["cat", "kairos"], t),
        );
        b.text(
            "service/systemctl-show.txt",
            &bundle::command_output(
                "systemctl",
                &["show", "kairos", "-p", "ActiveState,SubState,MainPID,ExecMainStartTimestamp,NRestarts,LimitNOFILE,User,Group,WorkingDirectory"],
                t,
            ),
        );
        b.text(
            "service/journal.txt",
            &bundle::command_output(
                "journalctl",
                &["-u", "kairos", "-n", "200", "--no-pager", "-o", "short-iso"],
                t,
            ),
        );
        b.section("service", "ok", "systemd 的設定與最近 200 行 journal");
    } else {
        b.section(
            "service",
            "skipped",
            "不是以 systemd 執行（未收集 unit 與 journal）",
        );
    }

    // 檔案清單（不含內容）
    let mut paths = Vec::new();
    if let Some(f) = &report.config_file {
        paths.push(f.clone());
    }
    paths.push(PathBuf::from(crate::logger::LOG_DIR));
    if let Some(c) = &report.config {
        paths.push(PathBuf::from(&c.storage.data_dir));
        for p in [&c.server.cert_path, &c.server.key_path] {
            paths.push(PathBuf::from(p));
        }
        if let Some(ca) = c.auth.client_ca_path() {
            paths.push(PathBuf::from(ca));
        }
        paths.extend(c.auth.crl_paths().into_iter().map(PathBuf::from));
    }
    b.text("files.txt", &bundle::file_listing(&paths));

    // 摘要（給支援人員第一眼看）
    let mut summary = format!(
        "{}\n產生時間：{}（UTC）\n主機：{}\n\n",
        crate::VERSION,
        crate::storage::fmt_time(now),
        host
    );
    for s in &b.sections {
        summary.push_str(&format!("[{}] {}：{}\n", s.result, s.id, s.note));
    }
    summary.push('\n');
    summary.push_str(&check_text);
    b.text("summary.txt", &summary);
    (b, report)
}

fn sha256_hex(data: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, data)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 加上 MANIFEST.json 與 REDACTION.txt，並做最後的秘密檢查；回傳 tar.gz
pub fn finish(b: Bundle, args: &DiagArgs, now: DateTime<Utc>) -> Result<Vec<u8>, String> {
    b.finish(args.redact_hosts, |tar, b| {
        let files: Vec<Value> = b
            .tar
            .files()
            .iter()
            .map(|(p, n)| json!({"path": p, "bytes": n}))
            .collect();
        json!({
            "schema": "kairos.diag/v1",
            "generated_at": crate::storage::fmt_time(now),
            "version": env!("CARGO_PKG_VERSION"),
            "commit": env!("KAIROS_BUILD_COMMIT"),
            "redaction": {"hosts": args.redact_hosts, "removed": b.removed_json()},
            "sections": b.sections.iter().map(bundle::Section::to_json).collect::<Vec<_>>(),
            "files": files,
            "tar_sha256": sha256_hex(tar),
        })
    })
}

/// `kairos diag`
pub fn run(args: DiagArgs) -> u8 {
    let now = Utc::now();
    let host = kp_obs::logfmt::hostname();
    let output = args.output.clone().unwrap_or_else(|| {
        PathBuf::from(format!(
            "kairos-diag-{}-{}.tar.gz",
            host.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "_"),
            now.format("%Y%m%dT%H%M%SZ")
        ))
    });
    if !args.json {
        eprintln!("{} · 收集診斷資訊…", crate::VERSION);
    }
    let (bundle, report) = build(&args, now);
    let mapping = bundle
        .pseudonyms
        .as_ref()
        .map(|p| p.mapping().clone())
        .unwrap_or_default();
    let sections = bundle.sections.clone();
    let truncated = bundle.truncated;
    let counts = bundle.counts.clone();
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
                    json!({"schema": "kairos.diag/v1", "ok": false, "exit_code": 1,
                        "error": {"code": CODE_DIAG_FAILED, "message": e}})
                );
            } else {
                eprintln!("✗ 錯誤 {CODE_DIAG_FAILED}：無法產生診斷包\n  原因：{e}");
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
    let warnings = truncated || sections.iter().any(|s| s.result == "warning");
    let code = if report.count(Outcome::Error) > 0 {
        9
    } else if warnings {
        8
    } else {
        0
    };
    if args.json {
        println!(
            "{}",
            json!({
                "schema": "kairos.diag/v1",
                "ok": true,
                "generated_at": crate::storage::fmt_time(now),
                "exit_code": code,
                "data": {
                    "output": output.display().to_string(),
                    "size_bytes": data.len(),
                    "sha256": sha256_hex(&data),
                    "redaction": {"hosts": args.redact_hosts, "removed": counts.items().iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>(),
                        "mapping_file": mapping_file.as_ref().map(|p| p.display().to_string())},
                    "sections": sections.iter().map(bundle::Section::to_json).collect::<Vec<_>>(),
                },
                "warnings": [],
                "error": null,
            })
        );
    } else {
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
        if let Some(p) = &mapping_file {
            println!("  代號對照表：{}（不在診斷包中，請自行保管）", p.display());
        } else if !args.redact_hosts {
            println!(
                "  遮蔽：token、密碼、私鑰已移除；主機名稱與 IP 保留（若需要隱藏，請改用 --redact-hosts）"
            );
        }
        println!(
            "  下一步：把這個檔案交給支援人員。檔案內容可以先解開檢查（summary.txt 是摘要）。"
        );
        if truncated {
            println!(
                "\n⚠ 警告 {CODE_DIAG_TRUNCATED}：日誌超過大小上限，已截斷\n  原因：要求收集的日誌超過 {} MB，只收集了較新的部分。\n  處理方式：以 --since 指定較短的期間，或直接提供 logs/ 中特定日期的檔案。",
                MAX_LOG_BYTES / 1024 / 1024
            );
        }
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use kp_obs::redact;
    use std::io::Write;

    #[test]
    fn args() {
        let a = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        let d = parse_args(a(&[
            "--since",
            "24h",
            "--redact-hosts",
            "-o",
            "x.tar.gz",
            "--no-server",
        ]))
        .unwrap();
        assert_eq!(d.since, Duration::from_secs(86_400));
        assert!(d.redact_hosts && d.no_server);
        assert_eq!(d.output, Some(PathBuf::from("x.tar.gz")));
        assert!(parse_args(a(&["--since", "0h"])).is_err());
        assert!(parse_args(a(&["--bogus"])).is_err());
        let c = parse_check_args(a(&["--strict", "--json", "--config", "k.toml"])).unwrap();
        assert!(c.strict && c.json);
        assert_eq!(c.config_path, Some(PathBuf::from("k.toml")));
        assert!(parse_check_args(a(&["--config", "a", "--config", "b"])).is_err());
        assert_eq!(
            crate::config::parse_args(a(&["--check-config", "--config", "k.toml"])).unwrap(),
            crate::config::CliAction::CheckConfig(CheckArgs {
                config_path: Some(PathBuf::from("k.toml")),
                strict: false,
                json: false
            })
        );
        assert!(matches!(
            crate::config::parse_args(a(&["diag", "--no-server"])).unwrap(),
            crate::config::CliAction::Diag(_)
        ));
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "kairos-diag-{tag}-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

    fn write_config(dir: &Path, extra: &str) -> PathBuf {
        let p = dir.join("kairos.toml");
        std::fs::write(
            &p,
            format!(
                "[server]\nlisten = \"127.0.0.1:0\"\ncert_path = \"{MANIFEST}/testdata/pki/server.crt\"\nkey_path = \"{MANIFEST}/testdata/pki/server.key\"\n\n[storage]\ndata_dir = \"{}\"\n\n[console]\nenabled = false\n{extra}",
                dir.join("data").display().to_string().replace('\\', "/")
            ),
        )
        .unwrap();
        p
    }

    #[test]
    fn check_config_reports_ok_and_errors() {
        let dir = temp("check");
        let cfg = write_config(&dir, "");
        let r = check(Some(&cfg));
        assert_eq!(r.count(Outcome::Error), 0, "{}", r.render_text());
        assert_eq!(r.exit_code(false), 0);
        let text = r.render_text();
        assert!(text.contains("✓ 通過") && text.contains("結果："), "{text}");
        let v = r.to_json(false);
        assert_eq!(v["schema"], "kairos.check-config/v1");
        assert!(v["data"]["checks"].as_array().unwrap().len() >= 6);

        // 範圍錯誤：結束代碼 3
        let bad = write_config(&dir, "[log]\nformat = \"xml\"\n");
        let r = check(Some(&bad));
        assert_eq!(r.exit_code(false), 3, "{}", r.render_text());
        assert!(r.render_text().contains("log.format"));
        // 語法錯誤
        std::fs::write(&bad, "[server\nlisten=").unwrap();
        let r = check(Some(&bad));
        assert_eq!(r.checks[0].code, Some(CODE_CONFIG_LOAD));
        assert_eq!(r.exit_code(false), 3);
        // 指定的設定檔不存在：KRS-E0401
        let r = check(Some(&dir.join("no-such.toml")));
        assert_eq!(r.checks[0].code, Some(CODE_CONFIG_MISSING));
        assert_eq!(r.exit_code(false), 3);
        // 無法讀取（權限）：KRS-E0402（以 root 執行時讀得到，略過）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = dir.join("locked.toml");
            std::fs::write(&locked, "").unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            if std::fs::read(&locked).is_err() {
                let r = check(Some(&locked));
                assert_eq!(
                    r.checks[0].code,
                    Some(CODE_CONFIG_UNREADABLE),
                    "{}",
                    r.render_text()
                );
                assert_eq!(r.exit_code(false), 3);
            }
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        // 憑證錯誤：結束代碼 4
        std::fs::write(
            &bad,
            format!("[server]\nlisten = \"127.0.0.1:0\"\ncert_path = \"{}\"\nkey_path = \"{}\"\n[console]\nenabled = false\n[storage]\ndata_dir = \"{}\"\n",
                dir.join("missing.crt").display().to_string().replace('\\', "/"),
                dir.join("missing.key").display().to_string().replace('\\', "/"),
                dir.join("data").display().to_string().replace('\\', "/")),
        )
        .unwrap();
        let r = check(Some(&bad));
        assert_eq!(r.exit_code(false), 4, "{}", r.render_text());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diag_bundle_contains_no_secrets() {
        let dir = temp("bundle");
        // 設定中放秘密：token 檔（路徑保留）、syslog 以外的秘密值、註冊 token
        let token_file = dir.join("metrics.token");
        std::fs::write(&token_file, "0123456789abcdef0123456789abcdef\n").unwrap();
        let cfg = write_config(
            &dir,
            &format!(
                "\n[observability]\nmetrics_token_file = \"{}\"\n# 註解中的 token：KRT1-7G4Q-M2XD-9KQP-A1B2 與 kat_3f9c2a71b4e05d18_c2VjcmV0c2VjcmV0c2VjcmV0\n",
                token_file.display().to_string().replace('\\', "/")
            ),
        );
        // 日誌中混入私鑰與 Bearer（模擬誤記錄）
        let logs = Path::new(crate::logger::LOG_DIR);
        let _ = std::fs::create_dir_all(logs);
        let log_name = format!("kairos-{}.log", Utc::now().format("%Y-%m-%d"));
        let log_path = logs.join(&log_name);
        let before = std::fs::read(&log_path).ok();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        writeln!(
            f,
            "2026-10-14T06:00:00.000Z [WARN] 測試 Authorization: Bearer abcdefSECRET 與\n-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49\n-----END PRIVATE KEY-----"
        )
        .unwrap();
        // 0.6 的告警與帳號：密碼雜湊、TOTP、工作階段、CSRF、SMTP 密碼、Webhook 網址的秘密（模擬誤記錄）
        writeln!(
            f,
            "2026-10-14T06:00:01.000Z [WARN] 測試 hash=$argon2id$v=19$m=19456,t=2,p=1$U0FMVFNBTFRTQUxU$SEFTSEhBU0hIQVNISEFTSEhBU0g otpauth://totp/Kairos:ann?secret=TOTPSECRETBASE32 Cookie: __Host-kairos_session=SESSIONSECRETxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx X-CSRF-Token: CSRFSECRET0123456789\n{{\"secrets\": {{\"password\": \"SMTPPASSWORD\", \"hmac_secret\": \"HMACSECRET\"}}}}\nWebhook https://hooks.slack.com/services/T0000/B0000/WEBHOOKSECRETWEBHOOKSECRET 與 http://siem.example.com/in?token=URLTOKENSECRET"
        )
        .unwrap();
        drop(f);
        // 資料目錄中的金鑰檔與資料庫（使用者、工作階段、通知管道）：只列檔名，內容不收集
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(
            data.join(crate::alerts::secrets::KEY_FILE),
            b"ALERTKEYALERTKEYALERTKEYALERTKEY",
        )
        .unwrap();
        std::fs::write(
            data.join(crate::users::secrets::KEY_FILE),
            b"CONSOLEKEYCONSOLEKEYCONSOLEKEYCO",
        )
        .unwrap();
        std::fs::write(
            data.join(crate::audit::KEY_FILE),
            b"AUDITHMACKEYAUDITHMACKEYAUDITHMA",
        )
        .unwrap();
        {
            let mut conn =
                rusqlite::Connection::open(data.join(crate::store::sqlite::DB_FILE)).unwrap();
            crate::store::sqlite::migrate(&mut conn, crate::store::sqlite::MIGRATIONS).unwrap();
            conn.execute(
                "INSERT INTO users (username, source, password_hash, totp_secret, created_at, created_by, updated_at)
                 VALUES ('ann', 'local', '$argon2id$v=19$m=19456,t=2,p=1$REJTQUxUREJTQUxU$REJIQVNIREJIQVNIREJIQVNIREJIQVNI', X'544f545044424c4f42', 0, 't', 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('test_secret', 'DBMETASECRET')",
                [],
            )
            .unwrap();
        }
        let args = DiagArgs {
            config_path: Some(cfg),
            no_server: true,
            ..DiagArgs::default()
        };
        let now = Utc::now();
        let (b, report) = build(&args, now);
        assert_eq!(report.count(Outcome::Error), 0, "{}", report.render_text());
        let tgz = finish(b, &args, now).expect("應可產生");
        // 還原（測試用的 inflate）後搜尋秘密樣式
        let tar = kp_obs::archive::gunzip(&tgz).expect("診斷包應可解開");
        let text = String::from_utf8_lossy(&tar);
        assert!(redact::find_leak(&text).is_none());
        for secret in [
            "0123456789abcdef0123456789abcdef",
            "abcdefSECRET",
            "MIGHAgEAMBMGByqGSM49",
            "c2VjcmV0c2VjcmV0c2VjcmV0",
            "M2XD-9KQP",
            // 告警與帳號（0.6）
            "ALERTKEYALERTKEY",
            "CONSOLEKEYCONSOLEKEY",
            "AUDITHMACKEYAUDITHMAC",
            "SEFTSEhBU0hIQVNI",
            "U0FMVFNBTFRTQUxU",
            "REJIQVNIREJIQVNI",
            "TOTPSECRETBASE32",
            "SESSIONSECRET",
            "CSRFSECRET",
            "SMTPPASSWORD",
            "HMACSECRET",
            "WEBHOOKSECRET",
            "URLTOKENSECRET",
            "DBMETASECRET",
        ] {
            assert!(!text.contains(secret), "診斷包中不應有 {secret}");
        }
        // 金鑰檔只出現在檔案清單（名稱），沒有內容
        assert!(
            text.contains("alerts-secrets.key"),
            "金鑰檔的名稱應列在 files.txt"
        );
        assert!(text.contains("kairos-diag/MANIFEST.json"));
        assert!(text.contains("kairos-diag/config/kairos.toml"));
        assert!(text.contains("metrics.token"), "token 檔的路徑保留");
        assert!(text.contains("\"schema\": \"kairos.diag/v1\""));
        // 復原日誌檔
        match before {
            Some(b) => std::fs::write(&log_path, b).unwrap(),
            None => {
                let _ = std::fs::remove_file(&log_path);
            }
        }
        // 寫出的檔案為 0600、不覆寫
        let out = dir.join("x.tar.gz");
        bundle::write_private(&out, b"x").unwrap();
        assert!(bundle::write_private(&out, b"y").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&out).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
