//! `kairos releases list|import|delete`：Pulse 發佈版本庫的命令列（REMOTE.md 第 4.1.1、8.2 節）。
//!
//! 與主控台 API 用同一套操作（[`crate::remote::RemoteService`] 的 `import_release`、`store_artifact`、
//! `delete_release`）：簽章與雜湊的驗證、`release_in_use` 的保護與稽核事件都相同，執行者為
//! `cli:<OS 帳號>`。版本庫只有資料庫的列與 `<releases_dir>/<sha256>` 的檔案，沒有記憶體中的狀態，Kairos
//! 執行中也可以直接操作（與 `kairos users`、`kairos enroll-token` 相同：直接開資料庫，WAL）；離線環境可以在
//! Kairos 啟動前先匯入。輸出依設計包 `design-system/cli-style.md`（結果在 stdout、錯誤在 stderr，`--json`
//! 為純 ASCII 的單一 JSON 文件）。

use chrono::{DateTime, FixedOffset, Local, Offset, Utc};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::api::http::ApiError;
use crate::config::{AppConfig, take_config_arg};
use crate::hosts_cli::{ascii_json, use_unicode};
use crate::remote::ops::{self, Actor};
use crate::remote::releases::{self, Upload};
use crate::remote::{RemoteService, StartParams};
use crate::storage::fmt_time;
use crate::store::sqlite::SqliteOptions;
use crate::token_cli::{
    Failure, Output, db_failure, human_time, open, operator, table, zone_label,
};

/// `kairos releases --help`
pub const RELEASES_USAGE: &str = "kairos releases — Pulse 發佈版本庫（遠端升級，crates/kairos-proto/REMOTE.md 第 4.1 節）

用法：
  kairos releases list [--json]
  kairos releases import <目錄> [--json]
  kairos releases delete <版本> --confirm <版本> [--reason <原因>] [--json]

  list     列出版本庫的版本、發佈檔與使用情形；標出已存入但檔案遺失或大小不符的發佈檔
  import   匯入同一次發佈：讀取 <目錄> 中的 SHA256SUMS、SHA256SUMS.sig、pulse-*-release.json（0.9.0 起的
           發佈都有）與 pulse-* 發佈檔（tar.gz、deb、rpm、msi、zip；不必全部，之後可以再執行一次補上）。
           以 [remote] allowed_signers 驗證簽章與雜湊（沒有設定時只檢查簽章本身與雜湊，並提出警告）
  delete   刪除一個版本與它的發佈檔；有推送以它為目標、或它是某些主機的回滾目標時拒絕

選項：
  --config <path>   Kairos 的設定檔（用來找到 storage.data_dir 與 [remote]；預設為目前目錄的 kairos.toml）
  --confirm <版本>  delete：再輸入一次版本號（L2 確認）
  --reason <原因>   delete：寫入稽核日誌的原因
  --external <URL>  import：外部 URL 的版本庫（只登記 SHA256SUMS、簽章與清單，Kairos 不保存發佈檔；Pulse 從
                    <URL><檔名> 下載，URL 必須在每台 Pulse 本機的 upgrade.url_allowlist 中；https://…/）
  --json            輸出機器可讀的 JSON（純 ASCII，schema kairos.releases.<命令>/v1）
  --ascii           不使用 Unicode 符號（也可設定環境變數 KAIROS_SYMBOLS=ascii）
  -h, --help        顯示本說明

範例：
  sudo -u kairos /opt/kairos/kairos releases import /tmp/pulse-0.9.1 --config /opt/kairos/kairos.toml
  kairos releases list
  kairos releases delete 0.9.0 --confirm 0.9.0 --reason \"所有主機都已升級\"

請以 Kairos 的服務帳號執行（版本庫的檔案必須屬於它）。Kairos 執行中也可以使用，不需要重新啟動。

結束代碼：0 正常、1 失敗（驗證不通過、無法讀寫）、2 用法錯誤、3 設定錯誤、10 權限不足、11 衝突（版本使用中）";

const EXIT_GENERAL: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_CONFIG: u8 = 3;
const EXIT_PERMISSION: u8 = 10;
const EXIT_CONFLICT: u8 = 11;

/// 單一文字檔（`SHA256SUMS`、簽章、清單）的讀取上限
const MAX_TEXT_BYTES: u64 = 64 * 1024;

/// `kairos releases` 的動作
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleasesAction {
    Help,
    List,
    Import {
        dir: PathBuf,
        /// `--external <base_url>`：外部 URL 的版本庫（REMOTE.md 第 4.1.2 節；只登記簽章資料，不匯入發佈檔）
        external: Option<String>,
    },
    Delete {
        version: String,
        confirm: Option<String>,
        reason: Option<String>,
    },
}

/// `kairos releases` 的命令
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasesCommand {
    pub action: ReleasesAction,
    pub config_path: Option<PathBuf>,
    pub json: bool,
    pub ascii: bool,
}

fn text(a: OsString, what: &str) -> Result<String, String> {
    a.into_string()
        .map_err(|_| format!("{what} 必須是 UTF-8 文字"))
}

/// 解析 `kairos releases` 之後的參數
pub fn parse_args(args: impl Iterator<Item = OsString>) -> Result<ReleasesCommand, String> {
    let mut args = args.peekable();
    let sub = match args.next() {
        None => return Err("kairos releases 需要子命令（list、import、delete）".to_string()),
        Some(a) if a == "-h" || a == "--help" => "help".to_string(),
        Some(a) => text(a, "子命令")?,
    };
    if !matches!(sub.as_str(), "help" | "list" | "import" | "delete") {
        return Err(format!("無法辨識的子命令: {sub}（list、import、delete）"));
    }
    let (mut config_path, mut json, mut ascii, mut help) = (None, false, false, sub == "help");
    let (mut positional, mut confirm, mut reason, mut external) = (Vec::new(), None, None, None);
    while let Some(arg) = args.next() {
        if let Some(p) = take_config_arg(&arg, &mut args)? {
            if config_path.replace(p).is_some() {
                return Err("--config 只能指定一次".to_string());
            }
            continue;
        }
        let mut value = |name: &str| -> Result<String, String> {
            match args.next() {
                Some(v) if !v.is_empty() => text(v, name),
                _ => Err(format!("{name} 需要一個值")),
            }
        };
        match arg.to_str() {
            Some("-h" | "--help") => help = true,
            Some("--json") => json = true,
            Some("--ascii") => ascii = true,
            Some("--confirm") if sub == "delete" => confirm = Some(value("--confirm")?),
            Some("--reason") if sub == "delete" => reason = Some(value("--reason")?),
            Some("--external") if sub == "import" => external = Some(value("--external")?),
            Some(a) if a.starts_with('-') => return Err(format!("無法辨識的選項: {a}")),
            _ => positional.push(arg),
        }
    }
    let action = if help {
        ReleasesAction::Help
    } else {
        let mut positional = positional.into_iter();
        let first = positional.next();
        if positional.next().is_some() {
            return Err(format!("kairos releases {sub} 的參數太多"));
        }
        match (sub.as_str(), first) {
            ("list", None) => ReleasesAction::List,
            ("list", Some(_)) => return Err("kairos releases list 不接受參數".to_string()),
            ("import", Some(d)) => ReleasesAction::Import {
                dir: PathBuf::from(d),
                external,
            },
            ("import", None) => {
                return Err("kairos releases import 需要發佈檔所在的目錄".to_string());
            }
            ("delete", Some(v)) => {
                let version = text(v, "版本")?;
                if reason
                    .as_deref()
                    .is_some_and(|r: &str| r.chars().any(kairos_proto::is_forbidden_display_char))
                {
                    return Err("--reason 不可有控制字元".to_string());
                }
                ReleasesAction::Delete {
                    version,
                    confirm,
                    reason,
                }
            }
            _ => return Err("kairos releases delete 需要版本號".to_string()),
        }
    };
    Ok(ReleasesCommand {
        action,
        config_path,
        json,
        ascii,
    })
}

fn schema(action: &ReleasesAction) -> &'static str {
    match action {
        ReleasesAction::Help | ReleasesAction::List => "kairos.releases.list/v1",
        ReleasesAction::Import { .. } => "kairos.releases.import/v1",
        ReleasesAction::Delete { .. } => "kairos.releases.delete/v1",
    }
}

/// API 的錯誤轉成命令列的失敗（三段式；結束代碼依 HTTP 狀態）
fn api_failure(title: &str, e: ApiError) -> Failure {
    let (exit, fix) = match e.code {
        "release_in_use" => (
            EXIT_CONFLICT,
            vec![
                "等使用這個版本的推送完成或取消；仍有主機執行這個版本（deb／rpm 的回滾需要它）時請保留。".to_string(),
                "kairos releases list 的「執行中」「回滾目標」「推送」欄列出使用情形。".to_string(),
            ],
        ),
        "release_not_found" => (
            EXIT_GENERAL,
            vec!["以 kairos releases list 確認版本號。".to_string()],
        ),
        "release_signature_invalid" => (
            EXIT_GENERAL,
            vec![
                "確認 SHA256SUMS 與 SHA256SUMS.sig 來自同一次官方發佈、下載後沒有被修改。".to_string(),
                "確認 [remote] allowed_signers 列出了簽署這次發佈的公鑰（RELEASING.md 第 3 節；客戶自行重新簽署時另設 extra_principals）。".to_string(),
            ],
        ),
        "release_invalid" | "artifact_not_listed" | "artifact_hash_mismatch" => (
            EXIT_GENERAL,
            vec!["重新下載這次發佈的所有檔案（SHA256SUMS、SHA256SUMS.sig、pulse-*-release.json 與發佈檔）後再匯入。".to_string()],
        ),
        "trust_anchor_unavailable" => (
            EXIT_CONFIG,
            vec!["修正 [remote] allowed_signers 指向的檔案（ssh-keygen 的 allowed_signers 格式），或清空這個設定（匯入時不驗證簽署者）。".to_string()],
        ),
        _ if e.status.is_server_error() => (
            EXIT_GENERAL,
            vec!["確認資料庫與版本庫目錄可以寫入；以 Kairos 的服務帳號執行。".to_string()],
        ),
        _ => (EXIT_GENERAL, Vec::new()),
    };
    Failure {
        code: e.product_code,
        ..Failure::new(title, e.message, fix, exit)
    }
}

fn io_failure(title: &str, path: &Path, e: &std::io::Error) -> Failure {
    let exit = if e.kind() == ErrorKind::PermissionDenied {
        EXIT_PERMISSION
    } else {
        EXIT_GENERAL
    };
    Failure::new(
        title,
        format!("{}：{e}", path.display()),
        vec!["確認路徑正確，且執行命令的帳號可以讀取。".to_string()],
        exit,
    )
}

/// 讀取小的文字檔（上限 [`MAX_TEXT_BYTES`]）；不存在時 `Ok(None)`
fn read_text(path: &Path) -> Result<Option<String>, Failure> {
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_failure("無法讀取發佈的檔案", path, &e)),
    };
    let mut buf = String::new();
    f.take(MAX_TEXT_BYTES + 1)
        .read_to_string(&mut buf)
        .map_err(|e| io_failure("無法讀取發佈的檔案", path, &e))?;
    if buf.len() as u64 > MAX_TEXT_BYTES {
        return Err(Failure::new(
            "無法讀取發佈的檔案",
            format!("{} 超過 {MAX_TEXT_BYTES} bytes", path.display()),
            vec!["確認目錄中是 Pulse 的官方發佈檔。".to_string()],
            EXIT_GENERAL,
        ));
    }
    Ok(Some(buf))
}

/// 目錄中的發佈清單（`pulse-*-release.json`；多於一個時錯誤）
fn find_manifest(dir: &Path) -> Result<Option<String>, Failure> {
    let entries = std::fs::read_dir(dir).map_err(|e| io_failure("無法讀取目錄", dir, &e))?;
    let mut found: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("pulse-") && n.ends_with("-release.json")
        })
        .map(|e| e.path())
        .collect();
    found.sort();
    match found.as_slice() {
        [] => Ok(None),
        [one] => read_text(one),
        _ => Err(Failure::new(
            "目錄中有多於一個發佈清單",
            found
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("、"),
            vec!["一次匯入一個版本：每個版本的檔案放在各自的目錄。".to_string()],
            EXIT_USAGE,
        )),
    }
}

/// 把一個發佈檔串流寫入版本庫的暫存檔並計算 SHA-256
fn upload_file(
    src: &Path,
    releases_dir: &Path,
    max: u64,
) -> Result<(Upload, String, u64), Failure> {
    let mut f = std::fs::File::open(src).map_err(|e| io_failure("無法讀取發佈檔", src, &e))?;
    let mut up = Upload::create(releases_dir, max)
        .map_err(|e| io_failure("無法寫入版本庫", releases_dir, &e))?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_failure("無法讀取發佈檔", src, &e)),
        };
        match up.write(&buf[..n]) {
            Ok(()) => {}
            Err(None) => {
                return Err(Failure::new(
                    "發佈檔太大",
                    format!(
                        "{} 超過 remote.max_release_bytes（{max} bytes）",
                        src.display()
                    ),
                    vec![
                        "確認這是 Pulse 的官方發佈檔；必要時調高 [remote] max_release_bytes。"
                            .to_string(),
                    ],
                    EXIT_GENERAL,
                ));
            }
            Err(Some(e)) => return Err(io_failure("無法寫入版本庫", releases_dir, &e)),
        }
    }
    let (sha, size) = up
        .finish()
        .map_err(|e| io_failure("無法寫入版本庫", releases_dir, &e))?;
    Ok((up, sha, size))
}

fn size_text(bytes: i64) -> String {
    if bytes <= 0 {
        "—".to_string()
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    }
}

/// 版本清單（與 `GET /api/v1/releases` 相同的欄位，另加每個發佈檔的 `file_problem`）
async fn list(svc: &RemoteService) -> Result<Vec<Value>, Failure> {
    let dir = svc.releases_dir.clone();
    svc.db
        .read(move |s| {
            let hosts = s.upgrade_hosts()?;
            let mut out = Vec::new();
            for r in s.list_releases()? {
                let artifacts = s.release_artifacts(r.id)?;
                let on = hosts
                    .iter()
                    .filter(|h| h.agent_version.as_deref() == Some(r.version.as_str()))
                    .count();
                let rollback = ops::rollback_target_count(&hosts, &r.version, &artifacts);
                let rollouts = s.rollouts_using_release(r.id)?;
                let mut v = ops::release_json(&r, &artifacts, Some((on, rollback, rollouts)));
                if let Some(list) = v["artifacts"].as_array_mut() {
                    for (j, a) in list.iter_mut().zip(&artifacts) {
                        j["file_problem"] = json!(
                            a.stored
                                .then(|| releases::file_problem(&dir, &a.sha256, a.size))
                                .flatten()
                        );
                    }
                }
                out.push(v);
            }
            Ok(out)
        })
        .await
        .map_err(db_failure)
}

fn list_human(rows: &[Value], tz: FixedOffset, unicode: bool) -> String {
    if rows.is_empty() {
        return "版本庫沒有任何版本。以 kairos releases import <目錄> 匯入。\n".to_string();
    }
    let warn = if unicode { "⚠" } else { "[WARN]" };
    let mut t = vec![
        [
            "版本",
            "commit",
            "簽署金鑰",
            "發佈檔",
            "執行中",
            "回滾目標",
            "推送",
            "匯入時間",
        ]
        .map(String::from)
        .to_vec(),
    ];
    let mut notes = Vec::new();
    for r in rows {
        let arts = r["artifacts"].as_array().cloned().unwrap_or_default();
        let stored = arts.iter().filter(|a| a["stored"] == true).count();
        let version = r["version"].as_str().unwrap_or("");
        for a in &arts {
            let name = a["filename"].as_str().unwrap_or("");
            match a["file_problem"].as_str() {
                Some("size_mismatch") => notes.push(format!(
                    "{warn} {version} 的 {name}：檔案大小與記錄不符（重新匯入這個版本的目錄即可補上）"
                )),
                Some(_) => notes.push(format!(
                    "{warn} {version} 的 {name}：檔案遺失（重新匯入這個版本的目錄即可補上）"
                )),
                None => {}
            }
        }
        t.push(vec![
            version.to_string(),
            r["commit"].as_str().unwrap_or("").to_string(),
            r["signer_fingerprint"]
                .as_str()
                .map_or_else(|| "未驗證".to_string(), str::to_string),
            format!(
                "{stored}／{}（{}）",
                arts.len(),
                arts.iter()
                    .filter(|a| a["stored"] == true)
                    .map(|a| format!(
                        "{} {} {}",
                        a["format"].as_str().unwrap_or(""),
                        a["arch"].as_str().unwrap_or(""),
                        size_text(a["size"].as_i64().unwrap_or(0))
                    ))
                    .collect::<Vec<_>>()
                    .join("、")
            ),
            r["hosts_on_version"].to_string(),
            r["rollback_target_for"].to_string(),
            r["in_use_by_rollouts"].to_string(),
            DateTime::parse_from_rfc3339(r["imported_at"].as_str().unwrap_or(""))
                .map(|d| human_time(d.timestamp_millis(), tz))
                .unwrap_or_default(),
        ]);
    }
    let mut out = table(&t);
    out.push_str(&format!(
        "\n共 {} 個版本（時間為 {}）。「發佈檔」為已存入／SHA256SUMS 列出的數量。\n",
        rows.len(),
        zone_label(tz)
    ));
    for n in notes {
        out.push_str(&n);
        out.push('\n');
    }
    out
}

/// `import` 的結果
struct Imported {
    release: Value,
    /// 這次新建立了版本（否則是補上發佈檔）
    created: bool,
    stored: Vec<String>,
    already: Vec<String>,
    /// SHA256SUMS 列出、目錄中沒有的發佈檔
    absent: Vec<String>,
    warnings: Vec<String>,
}

async fn import(
    svc: &RemoteService,
    who: &Actor,
    dir: &Path,
    external: Option<&str>,
) -> Result<Imported, Failure> {
    if !dir.is_dir() {
        return Err(Failure::new(
            "找不到發佈檔的目錄",
            format!("{} 不是目錄", dir.display()),
            vec!["指定放有 SHA256SUMS、SHA256SUMS.sig 與 pulse-* 發佈檔的目錄。".to_string()],
            EXIT_USAGE,
        ));
    }
    let need = |name: &str| -> Result<String, Failure> {
        read_text(&dir.join(name))?.ok_or_else(|| {
            Failure::new(
                "目錄中缺少發佈的檔案",
                format!("{} 中沒有 {name}", dir.display()),
                vec!["一次發佈需要 SHA256SUMS、SHA256SUMS.sig（與 0.9.0 起的 pulse-*-release.json）以及要匯入的發佈檔。".to_string()],
                EXIT_GENERAL,
            )
        })
    };
    let sums = need("SHA256SUMS")?;
    let sig = need("SHA256SUMS.sig")?;
    let manifest = find_manifest(dir)?;
    let mut warnings = Vec::new();
    if svc.settings.allowed_signers.trim().is_empty() {
        warnings.push(
            "remote.allowed_signers 沒有設定：匯入時沒有驗證簽署者（只檢查簽章本身與雜湊）；每台 Pulse 仍以本機的信任錨驗證。"
                .to_string(),
        );
    }
    // 版本已在版本庫：SHA256SUMS 必須相同（同一次發佈），只補上發佈檔
    let (release, created) = match svc
        .import_release_from(
            who,
            sums.clone(),
            sig,
            manifest,
            external.map(str::to_string),
        )
        .await
    {
        Ok(v) => (v, true),
        Err(e) if e.code == "release_exists" => {
            // 版本號要從 SHA256SUMS 得知：以內容比對版本庫中的版本
            let same = svc
                .db
                .read(move |s| {
                    Ok(s.list_releases()?
                        .into_iter()
                        .find(|r| r.sha256sums == sums)
                        .map(|r| r.version))
                })
                .await
                .map_err(db_failure)?;
            let Some(version) = same else {
                return Err(Failure {
                    exit: EXIT_CONFLICT,
                    fix: vec![
                        "版本庫中同一個版本號的 SHA256SUMS 與這個目錄的不同（不是同一次發佈）。".to_string(),
                        "確認目錄的來源；要以這份取代時先 kairos releases delete <版本> --confirm <版本>。".to_string(),
                    ],
                    ..api_failure("版本庫已有這個版本", e)
                });
            };
            let row = list(svc)
                .await?
                .into_iter()
                .find(|r| r["version"] == version.as_str())
                .unwrap_or(Value::Null);
            (row, false)
        }
        Err(e) => return Err(api_failure("無法匯入發佈版本", e)),
    };
    let version = release["version"].as_str().unwrap_or("").to_string();
    let (mut stored, mut already, mut absent) = (Vec::new(), Vec::new(), Vec::new());
    // 外部 URL 的版本庫：Kairos 不保存發佈檔（由 Pulse 從 base_url 下載），只登記簽章資料
    let is_external = release["source"] == "external";
    if is_external {
        warnings.push(format!(
            "Pulse {version} 是外部 URL 的版本庫（{}）：目錄中的發佈檔只登記雜湊與大小，不存入 Kairos；請自行把它們放到這個 URL，並確認每台 Pulse 的 upgrade.url_allowlist 允許它。",
            release["base_url"].as_str().unwrap_or("")
        ));
    } else if external.is_some() {
        return Err(Failure::new(
            "版本庫已有這個版本",
            format!("Pulse {version} 已經以 Kairos 代管的方式匯入，不能改為外部 URL"),
            vec![format!(
                "要改為外部 URL：先 kairos releases delete {version} --confirm {version}，再以 --external 匯入。"
            )],
            EXIT_CONFLICT,
        ));
    }
    for a in release["artifacts"].as_array().cloned().unwrap_or_default() {
        let name = a["filename"].as_str().unwrap_or("").to_string();
        let src = dir.join(&name);
        if !src.is_file() {
            if a["stored"] == true || a["registered"] == true {
                already.push(name);
            } else {
                absent.push(name);
            }
            continue;
        }
        let ok = a["stored"] == true
            && releases::file_problem(
                &svc.releases_dir,
                a["sha256"].as_str().unwrap_or(""),
                a["size"].as_i64().unwrap_or(0),
            )
            .is_none();
        if ok {
            already.push(name);
            continue;
        }
        let (dirp, max) = (svc.releases_dir.clone(), svc.settings.max_release_bytes);
        let src2 = src.clone();
        let (up, sha, size) = tokio::task::spawn_blocking(move || upload_file(&src2, &dirp, max))
            .await
            .map_err(|e| db_failure(format!("背景工作失敗：{e}")))??;
        svc.store_artifact(who, version.clone(), name.clone(), up, sha, size)
            .await
            .map_err(|e| api_failure(&format!("無法匯入 {name}"), e))?;
        stored.push(name);
    }
    // 目錄中有、SHA256SUMS 沒有列出的 pulse-* 檔案：不匯入，提醒
    if let Ok(entries) = std::fs::read_dir(dir) {
        let listed: Vec<String> = release["artifacts"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(|a| a["filename"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut extra: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| {
                n.starts_with("pulse-") && !n.ends_with("-release.json") && !listed.contains(n)
            })
            .collect();
        extra.sort();
        if !extra.is_empty() {
            warnings.push(format!(
                "沒有匯入（不在 SHA256SUMS 中，或不是版本庫接受的 Pulse 發佈檔）：{}",
                extra.join("、")
            ));
        }
    }
    if stored.is_empty() && already.is_empty() {
        warnings.push(
            "目錄中沒有這個版本的任何發佈檔：版本已登記，但還不能推送；把發佈檔放進目錄後再執行一次。"
                .to_string(),
        );
    }
    Ok(Imported {
        release,
        created,
        stored,
        already,
        absent,
        warnings,
    })
}

/// 開啟版本庫（設定、資料庫、`RemoteService`；不啟動推送引擎）
async fn open_service(
    config_path: Option<&Path>,
    create: bool,
) -> Result<Option<Arc<RemoteService>>, Failure> {
    // 與其他命令相同的檢查（設定、SQLite、root、權限），並在需要時建立、遷移資料庫
    let Some(conn) = open(config_path, create)? else {
        return Ok(None);
    };
    drop(conn);
    let config = AppConfig::load(config_path).map_err(|e| {
        Failure::new(
            "設定檔無法載入",
            e.to_string(),
            vec!["修正設定檔後再執行，或以 --config 指定 Kairos 使用的設定檔。".to_string()],
            EXIT_CONFIG,
        )
    })?;
    let data_dir = PathBuf::from(&config.storage.data_dir);
    let svc = RemoteService::open(StartParams {
        settings: config.remote.clone(),
        db_path: SqliteOptions::from_config(&config, &data_dir).path,
        data_dir,
        registry: Arc::new(crate::registry::Registry::with_max_agents(1)),
        shutdown: Arc::new(crate::shutdown::Shutdown::new()),
        clock: crate::enroll::system_clock(),
    })
    .await
    .map_err(|e| {
        Failure::new(
            "無法開啟版本庫",
            e,
            vec!["以 Kairos 的服務帳號執行，並確認 [remote] releases_dir 可以寫入。".to_string()],
            EXIT_GENERAL,
        )
    })?;
    Ok(Some(svc))
}

/// 執行命令（不直接寫 stdout／stderr，方便測試）
pub fn execute(
    cmd: &ReleasesCommand,
    now: DateTime<Utc>,
    tz: FixedOffset,
    unicode: bool,
) -> Output {
    let schema = schema(&cmd.action);
    let fail = |f: Failure| Output {
        stdout: if cmd.json {
            f.json(schema, now)
        } else {
            String::new()
        },
        stderr: if cmd.json {
            String::new()
        } else {
            f.human(unicode)
        },
        exit: f.exit,
    };
    let ok_json = |data: Value, warnings: &[String]| Output {
        stdout: ascii_json(&json!({
            "schema": schema, "ok": true, "generated_at": fmt_time(now),
            "data": data, "warnings": warnings, "error": null,
        })) + "\n",
        ..Default::default()
    };
    let (mark, warn) = if unicode {
        ("✓", "⚠")
    } else {
        ("[ OK ]", "[WARN]")
    };
    match &cmd.action {
        ReleasesAction::Help => {
            return Output {
                stdout: format!("{RELEASES_USAGE}\n"),
                ..Default::default()
            };
        }
        ReleasesAction::Delete {
            version, confirm, ..
        } if confirm.as_deref() != Some(version.as_str()) => {
            return fail(Failure::new(
                "未執行",
                format!("刪除發佈版本需要確認（L2）：--confirm 必須等於版本號 {version}"),
                vec![format!(
                    "kairos releases delete {version} --confirm {version}"
                )],
                EXIT_USAGE,
            ));
        }
        _ => {}
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return fail(db_failure(format!("無法建立執行環境：{e}"))),
    };
    let who = Actor {
        actor: format!("cli:{}", operator()),
        ip: None,
    };
    let result: Result<Output, Failure> = rt.block_on(async {
        let creating = matches!(cmd.action, ReleasesAction::Import { .. });
        let Some(svc) = open_service(cmd.config_path.as_deref(), creating).await? else {
            return match &cmd.action {
                ReleasesAction::List if cmd.json => Ok(ok_json(json!({ "releases": [] }), &[])),
                ReleasesAction::List => Ok(Output {
                    stdout: list_human(&[], tz, unicode),
                    ..Default::default()
                }),
                _ => Err(db_failure("資料庫不存在（版本庫還沒有任何版本）")),
            };
        };
        match &cmd.action {
            ReleasesAction::List => {
                let rows = list(&svc).await?;
                Ok(if cmd.json {
                    ok_json(json!({ "releases": rows }), &[])
                } else {
                    Output {
                        stdout: list_human(&rows, tz, unicode),
                        ..Default::default()
                    }
                })
            }
            ReleasesAction::Import { dir, external } => {
                let r = import(&svc, &who, dir, external.as_deref()).await?;
                if cmd.json {
                    return Ok(ok_json(
                        json!({
                            "version": r.release["version"],
                            "commit": r.release["commit"],
                            "created": r.created,
                            "verified": r.release["verified"],
                            "signer_fingerprint": r.release["signer_fingerprint"],
                            "source": r.release["source"],
                            "base_url": r.release["base_url"],
                            "stored": r.stored,
                            "already_stored": r.already,
                            "not_in_directory": r.absent,
                        }),
                        &r.warnings,
                    ));
                }
                let version = r.release["version"].as_str().unwrap_or("");
                let mut out = format!(
                    "{mark} {} Pulse {version}（{}）：存入 {} 個發佈檔{}\n",
                    if r.created { "已匯入" } else { "已補上" },
                    r.release["signer_fingerprint"]
                        .as_str()
                        .map_or_else(|| "簽署者未驗證".to_string(), |f| format!("簽署金鑰 {f}")),
                    r.stored.len(),
                    if r.already.is_empty() {
                        String::new()
                    } else {
                        format!("，{} 個先前已存入", r.already.len())
                    }
                );
                for n in &r.stored {
                    out.push_str(&format!("  {n}\n"));
                }
                if !r.absent.is_empty() {
                    out.push_str(&format!(
                        "  SHA256SUMS 列出、目錄中沒有（需要時之後再匯入）：{}\n",
                        r.absent.join("、")
                    ));
                }
                Ok(Output {
                    stdout: out,
                    stderr: r.warnings.iter().map(|w| format!("{warn} {w}\n")).collect(),
                    exit: 0,
                })
            }
            ReleasesAction::Delete {
                version, reason, ..
            } => {
                svc.delete_release(&who, version.clone(), reason.clone())
                    .await
                    .map_err(|e| api_failure(&format!("無法刪除 Pulse {version}"), e))?;
                Ok(if cmd.json {
                    ok_json(json!({ "version": version, "deleted": true }), &[])
                } else {
                    Output {
                        stdout: format!("{mark} 已從版本庫刪除 Pulse {version}\n"),
                        ..Default::default()
                    }
                })
            }
            ReleasesAction::Help => unreachable!("已在前面處理"),
        }
    });
    result.unwrap_or_else(fail)
}

/// `kairos releases` 的進入點
pub fn run(cmd: ReleasesCommand) -> u8 {
    let unicode = !cmd.json && use_unicode(cmd.ascii);
    let out = execute(&cmd, Utc::now(), Local::now().offset().fix(), unicode);
    let mut stdout = std::io::stdout().lock();
    let wrote = stdout
        .write_all(out.stdout.as_bytes())
        .and_then(|_| stdout.flush());
    eprint!("{}", out.stderr);
    match wrote {
        Err(e) if e.kind() != ErrorKind::BrokenPipe => out.exit.max(EXIT_GENERAL),
        _ => out.exit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../crates/kairos-proto/testdata/release/"
    );
    const ARTIFACTS: [&str; 2] = [
        "pulse-0.9.1-5e6f7a8-linux-x86_64.deb",
        "pulse-0.9.1-5e6f7a8-linux-x86_64.tar.gz",
    ];

    fn os(args: &[&str]) -> impl Iterator<Item = OsString> {
        args.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// 暫存目錄、設定檔（`allowed_signers` 為測試向量的信任錨，`trust` 為 `false` 時不設定）
    fn setup(tag: &str, trust: bool) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "kairos-releases-cli-{tag}-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let fwd = |p: &Path| p.display().to_string().replace('\\', "/");
        let cfg = dir.join("kairos.toml");
        let signers = if trust {
            format!(
                "[remote]\nallowed_signers = \"{}\"\n",
                fwd(Path::new(&format!("{VECTORS}allowed_signers")))
            )
        } else {
            String::new()
        };
        std::fs::write(
            &cfg,
            format!(
                "[storage]\ndata_dir = \"{}\"\n{signers}",
                fwd(&dir.join("data"))
            ),
        )
        .unwrap();
        (dir, cfg)
    }

    /// 把測試向量的發佈複製到 `<dir>/<name>`（`files` 為要一起複製的發佈檔）
    fn bundle(dir: &Path, name: &str, files: &[&str]) -> PathBuf {
        let b = dir.join(name);
        std::fs::create_dir_all(&b).unwrap();
        for f in [
            "SHA256SUMS",
            "SHA256SUMS.sig",
            "pulse-0.9.1-5e6f7a8-release.json",
        ]
        .iter()
        .chain(files)
        {
            std::fs::copy(format!("{VECTORS}{f}"), b.join(f)).unwrap();
        }
        b
    }

    fn run_cmd(args: &[&str], cfg: &Path) -> Output {
        let mut all: Vec<&str> = args.to_vec();
        let c = cfg.to_string_lossy().into_owned();
        all.push("--config");
        all.push(&c);
        let cmd = parse_args(os(&all)).unwrap();
        execute(
            &cmd,
            Utc::now(),
            FixedOffset::east_opt(8 * 3600).unwrap(),
            false,
        )
    }

    fn json_of(out: &Output) -> Value {
        serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"))
    }

    fn audit_actions(cfg: &Path) -> Vec<(String, String)> {
        let conn = open(Some(cfg), false).unwrap().unwrap();
        let (events, _) = conn
            .audit_query(&crate::audit::AuditFilter::default(), 100)
            .unwrap();
        events.into_iter().map(|e| (e.action, e.actor)).collect()
    }

    #[test]
    fn argument_parsing() {
        assert_eq!(
            parse_args(os(&["list", "--json"])).unwrap(),
            ReleasesCommand {
                action: ReleasesAction::List,
                config_path: None,
                json: true,
                ascii: false,
            }
        );
        assert_eq!(
            parse_args(os(&["import", "/tmp/r", "--config", "k.toml"]))
                .unwrap()
                .action,
            ReleasesAction::Import {
                dir: PathBuf::from("/tmp/r"),
                external: None
            }
        );
        assert_eq!(
            parse_args(os(&[
                "import",
                "--external",
                "https://repo.example/pulse/",
                "d"
            ]))
            .unwrap()
            .action,
            ReleasesAction::Import {
                dir: PathBuf::from("d"),
                external: Some("https://repo.example/pulse/".into())
            }
        );
        assert_eq!(
            parse_args(os(&[
                "delete",
                "0.9.0",
                "--confirm",
                "0.9.0",
                "--reason",
                "舊版"
            ]))
            .unwrap()
            .action,
            ReleasesAction::Delete {
                version: "0.9.0".into(),
                confirm: Some("0.9.0".into()),
                reason: Some("舊版".into())
            }
        );
        assert_eq!(
            parse_args(os(&["--help"])).unwrap().action,
            ReleasesAction::Help
        );
        assert_eq!(
            parse_args(os(&["import", "--help"])).unwrap().action,
            ReleasesAction::Help
        );
        for bad in [
            &[][..],
            &["frobnicate"],
            &["list", "extra"],
            &["import"],
            &["import", "a", "b"],
            &["delete"],
            &["delete", "0.9.0", "--confirm"],
            &["list", "--confirm", "x"],
            &["delete", "0.9.0", "--external", "https://x/"],
            &["list", "--bogus"],
        ] {
            assert!(parse_args(os(bad)).is_err(), "{bad:?}");
        }
        assert!(RELEASES_USAGE.contains("kairos releases import"));
    }

    #[test]
    fn import_list_delete_with_audit() {
        let (dir, cfg) = setup("life", true);
        // 還沒有資料庫：list 是空的、delete 失敗
        let out = run_cmd(&["list", "--json"], &cfg);
        assert_eq!(out.exit, 0, "{out:?}");
        assert_eq!(json_of(&out)["data"]["releases"], json!([]));
        assert_eq!(
            run_cmd(&["delete", "0.9.1", "--confirm", "0.9.1"], &cfg).exit,
            EXIT_GENERAL
        );

        // 先匯入 deb，之後補上 tar.gz
        let first = bundle(&dir, "first", &ARTIFACTS[..1]);
        let out = run_cmd(&["import", first.to_str().unwrap(), "--json"], &cfg);
        assert_eq!(out.exit, 0, "{out:?}");
        let v = json_of(&out);
        assert_eq!(v["schema"], "kairos.releases.import/v1");
        assert_eq!(v["data"]["version"], "0.9.1");
        assert_eq!(v["data"]["created"], true);
        assert_eq!(v["data"]["verified"], true);
        assert_eq!(v["data"]["stored"], json!([ARTIFACTS[0]]));
        assert_eq!(v["data"]["not_in_directory"], json!([ARTIFACTS[1]]));
        assert_eq!(v["warnings"], json!([]));

        let all = bundle(&dir, "all", &ARTIFACTS);
        std::fs::write(all.join("pulse-9.9.9-0000000-linux-x86_64.deb"), b"x").unwrap();
        let out = run_cmd(&["import", all.to_str().unwrap()], &cfg);
        assert_eq!(out.exit, 0, "{out:?}");
        assert!(out.stdout.contains("已補上 Pulse 0.9.1"), "{out:?}");
        assert!(out.stdout.contains(ARTIFACTS[1]), "{out:?}");
        assert!(out.stdout.contains("1 個先前已存入"), "{out:?}");
        assert!(
            out.stderr.contains("pulse-9.9.9-0000000-linux-x86_64.deb"),
            "不在 SHA256SUMS 中的檔案要提醒：{out:?}"
        );

        let out = run_cmd(&["list", "--json"], &cfg);
        let v = json_of(&out);
        let rel = &v["data"]["releases"][0];
        assert_eq!(rel["version"], "0.9.1");
        assert_eq!(rel["artifacts"].as_array().unwrap().len(), 2);
        for a in rel["artifacts"].as_array().unwrap() {
            assert_eq!(a["stored"], true);
            assert_eq!(a["file_problem"], Value::Null);
        }
        let human = run_cmd(&["list"], &cfg);
        assert!(human.stdout.contains("0.9.1"), "{human:?}");
        assert!(human.stdout.contains("2／2"), "{human:?}");
        assert!(!human.stdout.contains("[WARN]"), "{human:?}");

        // 版本庫的檔案遺失、大小不符：list 標出；重新匯入補上
        let sha = |i: usize| rel["artifacts"][i]["sha256"].as_str().unwrap().to_string();
        let store = dir.join("data").join("releases");
        std::fs::remove_file(store.join(sha(0))).unwrap();
        std::fs::write(store.join(sha(1)), b"short").unwrap();
        let out = run_cmd(&["list", "--json"], &cfg);
        let v = json_of(&out);
        let arts = &v["data"]["releases"][0]["artifacts"];
        assert_eq!(arts[0]["file_problem"], "missing");
        assert_eq!(arts[1]["file_problem"], "size_mismatch");
        let human = run_cmd(&["list"], &cfg);
        assert!(human.stdout.contains("檔案遺失"), "{human:?}");
        assert!(human.stdout.contains("檔案大小與記錄不符"), "{human:?}");
        let out = run_cmd(&["import", all.to_str().unwrap(), "--json"], &cfg);
        assert_eq!(out.exit, 0, "{out:?}");
        assert_eq!(json_of(&out)["data"]["stored"].as_array().unwrap().len(), 2);
        let v = json_of(&run_cmd(&["list", "--json"], &cfg));
        for a in v["data"]["releases"][0]["artifacts"].as_array().unwrap() {
            assert_eq!(a["file_problem"], Value::Null, "{a}");
        }

        // 刪除：需要 --confirm 等於版本號
        assert_eq!(run_cmd(&["delete", "0.9.1"], &cfg).exit, EXIT_USAGE);
        assert_eq!(
            run_cmd(&["delete", "0.9.1", "--confirm", "0.9.0"], &cfg).exit,
            EXIT_USAGE
        );
        let out = run_cmd(
            &["delete", "0.9.1", "--confirm", "0.9.1", "--reason", "測試"],
            &cfg,
        );
        assert_eq!(out.exit, 0, "{out:?}");
        assert!(out.stdout.contains("已從版本庫刪除 Pulse 0.9.1"));
        assert_eq!(
            std::fs::read_dir(&store).unwrap().count(),
            0,
            "發佈檔一併刪除"
        );
        assert_eq!(
            run_cmd(&["delete", "0.9.1", "--confirm", "0.9.1"], &cfg).exit,
            EXIT_GENERAL,
            "已不存在"
        );

        // 稽核：執行者為 cli:<OS 帳號>
        let audit = audit_actions(&cfg);
        let count = |a: &str| audit.iter().filter(|(x, _)| x == a).count();
        // 建立版本 1 次＋發佈檔 1＋1＋2 次
        assert_eq!(count("release.import"), 5, "{audit:?}");
        assert_eq!(count("release.delete"), 1, "{audit:?}");
        assert!(
            audit.iter().all(|(_, who)| who.starts_with("cli:")),
            "{audit:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejected_imports_change_nothing_and_are_audited() {
        let (dir, cfg) = setup("bad", true);
        // 另一把金鑰簽的發佈
        let b = bundle(&dir, "other", &ARTIFACTS);
        std::fs::copy(
            format!("{VECTORS}SHA256SUMS.other-key.sig"),
            b.join("SHA256SUMS.sig"),
        )
        .unwrap();
        let out = run_cmd(&["import", b.to_str().unwrap(), "--json"], &cfg);
        assert_eq!(out.exit, EXIT_GENERAL, "{out:?}");
        let v = json_of(&out);
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], crate::remote::CODE_IMPORT_FAILED);
        let human = run_cmd(&["import", b.to_str().unwrap()], &cfg);
        assert!(human.stderr.contains("KRS-E8413"), "{human:?}");
        assert!(human.stdout.is_empty());

        // 發佈檔被改過：版本已登記，但這個檔案不存入
        let t = bundle(&dir, "tampered", &ARTIFACTS);
        std::fs::write(t.join(ARTIFACTS[0]), b"tampered").unwrap();
        let out = run_cmd(&["import", t.to_str().unwrap()], &cfg);
        assert_eq!(out.exit, EXIT_GENERAL, "{out:?}");
        assert!(out.stderr.contains("SHA-256"), "{out:?}");
        let v = json_of(&run_cmd(&["list", "--json"], &cfg));
        let arts = v["data"]["releases"][0]["artifacts"]
            .as_array()
            .unwrap()
            .clone();
        assert!(arts.iter().all(|a| a["stored"] == false), "{arts:?}");
        assert_eq!(
            std::fs::read_dir(dir.join("data").join("releases"))
                .unwrap()
                .count(),
            0,
            "暫存檔已清掉"
        );

        // 同一個版本號、不同的 SHA256SUMS：衝突
        let d = bundle(&dir, "different", &[]);
        let mut sums = std::fs::read_to_string(d.join("SHA256SUMS")).unwrap();
        sums.push_str(&format!(
            "{}  pulse-0.9.1-5e6f7a8-linux-aarch64.deb\n",
            "0".repeat(64)
        ));
        std::fs::write(d.join("SHA256SUMS"), sums).unwrap();
        // 簽章不符會先被擋下（不會走到衝突）；確認沒有改變版本庫
        assert_ne!(run_cmd(&["import", d.to_str().unwrap()], &cfg).exit, 0);

        // 缺少檔案、目錄不存在、--external
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(
            run_cmd(&["import", empty.to_str().unwrap()], &cfg).exit,
            EXIT_GENERAL
        );
        assert_eq!(
            run_cmd(&["import", dir.join("nope").to_str().unwrap()], &cfg).exit,
            EXIT_USAGE
        );
        let out = run_cmd(
            &[
                "import",
                t.to_str().unwrap(),
                "--external",
                "https://repo.example/p/",
            ],
            &cfg,
        );
        // 已經以代管的方式匯入的版本不能改為外部 URL
        assert_eq!(out.exit, EXIT_CONFLICT);
        assert!(out.stderr.contains("不能改為外部 URL"), "{out:?}");

        let audit = audit_actions(&cfg);
        assert!(
            audit
                .iter()
                .filter(|(a, _)| a == "release.import_failed")
                .count()
                >= 2,
            "{audit:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_without_trust_anchor_warns() {
        let (dir, cfg) = setup("notrust", false);
        let b = bundle(&dir, "b", &ARTIFACTS);
        let out = run_cmd(&["import", b.to_str().unwrap()], &cfg);
        assert_eq!(out.exit, 0, "{out:?}");
        assert!(out.stdout.contains("簽署者未驗證"), "{out:?}");
        assert!(
            out.stderr.contains("remote.allowed_signers 沒有設定"),
            "{out:?}"
        );
        let v = json_of(&run_cmd(&["list", "--json"], &cfg));
        assert_eq!(v["data"]["releases"][0]["verified"], false);
        let human = run_cmd(&["list"], &cfg);
        assert!(human.stdout.contains("未驗證"), "{human:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
