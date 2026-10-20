//! 遠端管理的命令（`crates/kairos-proto/REMOTE.md` 第 10.2 節）：
//!
//! - `pulse config show [--effective] [--sources] [--json] [-c <pulse.toml>]`：白名單鍵的值；`--effective`
//!   合併遠端設定快取，`--sources` 標出來源（`default`／`remote:<設定檔>`／`pulse.toml`／`env`）。
//! - `pulse upgrade status [--json] [-c <pulse.toml>]`：安裝方式、遠端升級的本機狀態、信任錨、進行中的升級與
//!   最近一次結果。
//! - `pulse upgrade apply --request <檔案>`：特權輔助程式的進入點（`pulse-upgrade.service`；內部使用）。
//! - `pulse upgrade rollback --yes`：tar 安裝手動換回 `pulse.prev`；deb／rpm 提示套件的回滾方式。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::cli::UsageError;
use crate::codes;
use crate::config_manager::AppConfig;

pub const CONFIG_USAGE: &str = "\
pulse config show — 顯示可由 Kairos 集中管理的設定（不連線）

用法：
  pulse config show [--effective] [--sources] [--json] [-c <pulse.toml>]

選項：
      --effective     合併遠端設定的快取（Pulse 實際使用的值）；省略時只看本機設定
      --sources       標出每個鍵的來源：default（內建預設）、remote:<設定檔>（Kairos 下發）、
                      pulse.toml、env（PULSE__* 環境變數）
      --json          輸出 JSON（schema pulse.config-show/v1）
  -c, --config <路徑> pulse.toml 的位置（預設依序找目前目錄、pulse 執行檔所在目錄）

合併順序（後者優先）：內建預設 < Kairos 下發 < pulse.toml < 環境變數（crates/kairos-proto/REMOTE.md 第 3.3 節）";

pub const UPGRADE_USAGE: &str = "\
pulse upgrade — 遠端升級的本機狀態與操作（Linux）

用法：
  pulse upgrade status [--json] [-c <pulse.toml>]
  pulse upgrade rollback --yes [-c <pulse.toml>]
  pulse upgrade apply --request <檔案>     （內部使用：pulse-upgrade.service 的進入點，需要 root）

命令：
  status     安裝方式、遠端升級是否就緒（與原因）、信任錨的金鑰指紋、進行中的升級與最近一次結果
  rollback   手動回滾：tar 安裝換回 /opt/pulse/pulse.prev 並重新啟動 pulse.service（需要 root）；
             deb／rpm 安裝顯示以套件管理員回滾的方式。危險操作，必須加 --yes";

/// `pulse config …`
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigShowArgs {
    pub effective: bool,
    pub sources: bool,
    pub json: bool,
    pub config: Option<String>,
}

/// `pulse upgrade …`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeArgs {
    Status { json: bool, config: Option<String> },
    Apply { request: PathBuf },
    Rollback { yes: bool, config: Option<String> },
}

fn take_value<'a>(
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

/// `pulse config <args>`；`Ok(None)` 為 `--help`
pub fn parse_config(args: &[String]) -> Result<Option<ConfigShowArgs>, UsageError> {
    if args.iter().any(|a| a == "-h" || a == "--help") || args.is_empty() {
        return if args.is_empty() {
            Err(UsageError::new(
                format!("{}：用法為 pulse config show [...]", codes::E_BAD_ARG_VALUE),
                CONFIG_USAGE,
            ))
        } else {
            Ok(None)
        };
    }
    if args[0] != "show" {
        return Err(UsageError::new(
            format!(
                "{}：pulse config 未知的命令「{}」（只有 show）",
                codes::E_UNKNOWN_ARG,
                args[0]
            ),
            CONFIG_USAGE,
        ));
    }
    let mut out = ConfigShowArgs::default();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--effective" => out.effective = true,
            "--sources" => out.sources = true,
            "--json" => out.json = true,
            "-c" | "--config" => {
                out.config = Some(take_value(args, &mut i, "--config", CONFIG_USAGE)?.to_string())
            }
            other => {
                return Err(UsageError::new(
                    format!(
                        "{}：pulse config show 未知的參數「{other}」",
                        codes::E_UNKNOWN_ARG
                    ),
                    CONFIG_USAGE,
                ));
            }
        }
        i += 1;
    }
    Ok(Some(out))
}

/// `pulse upgrade <args>`；`Ok(None)` 為 `--help`
pub fn parse_upgrade(args: &[String]) -> Result<Option<UpgradeArgs>, UsageError> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(None);
    }
    let err = |m: String| Err(UsageError::new(m, UPGRADE_USAGE));
    let Some(cmd) = args.first() else {
        return err(format!(
            "{}：用法為 pulse upgrade status|rollback",
            codes::E_BAD_ARG_VALUE
        ));
    };
    let mut json = false;
    let mut yes = false;
    let mut config = None;
    let mut request = None;
    let mut i = 1;
    while i < args.len() {
        match (cmd.as_str(), args[i].as_str()) {
            ("status", "--json") => json = true,
            ("rollback", "--yes") => yes = true,
            ("status" | "rollback", "-c" | "--config") => {
                config = Some(take_value(args, &mut i, "--config", UPGRADE_USAGE)?.to_string());
            }
            ("apply", "--request") => {
                request = Some(PathBuf::from(take_value(
                    args,
                    &mut i,
                    "--request",
                    UPGRADE_USAGE,
                )?));
            }
            (_, other) => {
                return err(format!(
                    "{}：pulse upgrade {cmd} 未知的參數「{other}」",
                    codes::E_UNKNOWN_ARG
                ));
            }
        }
        i += 1;
    }
    Ok(Some(match cmd.as_str() {
        "status" => UpgradeArgs::Status { json, config },
        "rollback" => UpgradeArgs::Rollback { yes, config },
        "apply" => match request {
            Some(request) => UpgradeArgs::Apply { request },
            None => {
                return err(format!(
                    "{}：pulse upgrade apply 需要 --request <檔案>",
                    codes::E_BAD_ARG_VALUE
                ));
            }
        },
        other => {
            return err(format!(
                "{}：pulse upgrade 未知的命令「{other}」",
                codes::E_UNKNOWN_ARG
            ));
        }
    }))
}

/// 找設定檔並載入（與 `pulse check-config` 相同的順序）；回傳 (設定, 設定檔)
fn load_config(explicit: Option<&str>) -> Result<(AppConfig, Option<PathBuf>), String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let located = crate::status_cmd::locate(explicit.map(Path::new), &cwd, exe_dir.as_deref());
    let path = located
        .config
        .clone()
        .unwrap_or_else(|| cwd.join(crate::config_manager::LOCAL_CONFIG_FILE));
    AppConfig::load_file(&path)
        .map(|c| (c, located.config))
        .map_err(|e| format!("{}：無法載入設定：{e}", codes::E_CONFIG_SYNTAX))
}

/// `pulse config show`
pub fn run_config(args: &ConfigShowArgs) -> u8 {
    let (local, file) = match load_config(args.config.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("錯誤 {e}");
            return 3;
        }
    };
    let sources = super::config::LocalSources::load(file.as_deref(), None);
    let cache = super::state_dir().join(super::config::CACHE_RELATIVE);
    let (merged, applied, cache_error) = if args.effective {
        match super::config::effective_from_cache(&local, &sources, &cache) {
            Ok((m, a)) => (m, a, None),
            Err(e) => (
                super::config::merge(
                    &local,
                    &AppConfig::embedded_defaults(),
                    &sources,
                    None,
                    None,
                ),
                None,
                Some(e),
            ),
        }
    } else {
        (
            super::config::merge(
                &local,
                &AppConfig::embedded_defaults(),
                &sources,
                None,
                None,
            ),
            None,
            None,
        )
    };
    let rows: Vec<(String, Value, String)> = kairos_proto::mgmt::settings::SETTINGS
        .iter()
        .map(|s| {
            (
                s.key.to_string(),
                kairos_proto::mgmt::settings::get(&merged.effective, s.key)
                    .cloned()
                    .unwrap_or(Value::Null),
                merged
                    .sources
                    .get(s.key)
                    .cloned()
                    .unwrap_or_else(|| "default".into()),
            )
        })
        .collect();
    if args.json {
        let keys: Vec<Value> = rows
            .iter()
            .map(|(k, v, s)| {
                if args.sources {
                    json!({"key": k, "value": v, "source": s})
                } else {
                    json!({"key": k, "value": v})
                }
            })
            .collect();
        let out = json!({
            "schema": "pulse.config-show/v1",
            "effective": args.effective,
            "remote_config_enabled": local.remote_config.enabled,
            "remote": applied.as_ref().map(|a| json!({"rev": a.rev, "hash": a.hash, "applied_at": a.applied_at})),
            "cache_error": cache_error,
            "overridden": merged.overridden.iter().map(|o| o.key.clone()).collect::<Vec<_>>(),
            "keys": keys,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return 0;
    }
    let width = rows.iter().map(|(k, ..)| k.len()).max().unwrap_or(0);
    if args.effective {
        match (&applied, &cache_error) {
            (_, Some(e)) => println!(
                "遠端設定快取無法使用（{}）：{e}；以下只有本機設定",
                codes::E_REMOTE_CONFIG_CACHE
            ),
            (Some(a), None) => println!(
                "集中設定：第 {} 版（{}），{} 個鍵由本機覆蓋",
                a.rev,
                a.applied_at,
                merged.overridden.len()
            ),
            (None, None) if !local.remote_config.enabled => {
                println!("集中設定：本機停用（remote_config.enabled = false）")
            }
            (None, None) => println!("集中設定：沒有下發的設定"),
        }
    }
    for (k, v, s) in &rows {
        if args.sources {
            println!("{k:<width$}  {v}  ({s})");
        } else {
            println!("{k:<width$}  {v}");
        }
    }
    0
}

/// `pulse upgrade …`
pub fn run_upgrade(args: &UpgradeArgs) -> u8 {
    match args {
        UpgradeArgs::Apply { request } => super::upgrade::helper::run(request),
        UpgradeArgs::Status { json, config } => upgrade_status(*json, config.as_deref()),
        UpgradeArgs::Rollback { yes, config } => rollback(*yes, config.as_deref()),
    }
}

fn upgrade_status(json_out: bool, config: Option<&str>) -> u8 {
    let (local, file) = match load_config(config) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("錯誤 {e}");
            return 3;
        }
    };
    // 與服務相同：[upgrade] 以 pulse.toml 為準，環境變數只能關閉
    let file_only = file
        .as_deref()
        .and_then(|p| AppConfig::load_file_only(p).ok())
        .unwrap_or_else(AppConfig::embedded_defaults);
    let anchor = file_only.upgrade.allowed_signers_path();
    let d = super::install::detect(
        local.upgrade.enabled && file_only.upgrade.enabled,
        &anchor,
        &super::install::Probe::for_cli(),
    );
    let state = super::upgrade::read_state(&super::state_dir());
    let (current, last) = match &state {
        Ok(s) => (s.current.clone(), s.history.last().cloned()),
        Err(_) => (None, None),
    };
    if json_out {
        let out = json!({
            "schema": "pulse.upgrade-status/v1",
            "version": env!("CARGO_PKG_VERSION"),
            "install": {"method": d.method, "upgrade": d.upgrade, "reason": d.reason},
            "trust_anchor": {"path": d.trust_anchor.display().to_string(), "fingerprints": d.fingerprints},
            "current": current.as_ref().map(|c| json!({
                "upgrade_id": c.upgrade_id,
                "phase": c.phase,
                "to_version": c.to_version,
                "apply_window": c.offer.apply_window,
            })),
            "last": last,
            "state_error": state.as_ref().err(),
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return 0;
    }
    println!(
        "Pulse {}（安裝方式：{}）",
        env!("CARGO_PKG_VERSION"),
        d.method
    );
    let readiness = match d.upgrade {
        "ready" => "就緒".to_string(),
        "disabled" => format!("本機停用（{}）", codes::I_UPGRADE_DISABLED),
        "no_trust_anchor" => format!("缺少信任錨（{}）", codes::E_UPGRADE_TRUST_ANCHOR),
        "no_helper" => format!("缺少輔助程式（{}）", codes::W_UPGRADE_UNSUPPORTED),
        other => format!("不支援（{other}，{}）", codes::W_UPGRADE_UNSUPPORTED),
    };
    println!("遠端升級：{readiness}");
    if let Some(r) = &d.reason {
        println!("  原因：{r}");
    }
    println!("信任錨：{}", d.trust_anchor.display());
    for f in &d.fingerprints {
        println!("  金鑰：{f}");
    }
    match &current {
        Some(c) => println!(
            "進行中：{} → {}（{:?}，時段 {}～{}）",
            c.from_version,
            c.to_version,
            c.phase,
            c.offer.apply_window.not_before,
            c.offer.apply_window.not_after
        ),
        None => println!("進行中：沒有"),
    }
    if let Some(l) = &last {
        println!(
            "最近一次：{} → {}：{}{}（{}）",
            l.from_version,
            l.to_version,
            l.state,
            l.code
                .as_deref()
                .map(|c| format!("，{c}"))
                .unwrap_or_default(),
            l.at
        );
    }
    if let Err(e) = &state {
        println!("（無法讀取遠端升級的狀態檔：{e}）");
    }
    0
}

fn rollback(yes: bool, config: Option<&str>) -> u8 {
    let local = match load_config(config) {
        Ok((c, _)) => c,
        Err(e) => {
            eprintln!("錯誤 {e}");
            return 3;
        }
    };
    let _ = local;
    let method = super::install::detect_method(&super::install::Probe::for_cli());
    match method {
        "deb" => {
            println!(
                "deb 安裝請以套件管理員回滾：取得目前部署前的版本的 .deb 後執行\n  sudo dpkg -i pulse-<版本>-<commit>-linux-<arch>.deb\n（dpkg 允許降版；先以 sha256sum -c、ssh-keygen -Y verify 驗證發佈包，見 RELEASING.md 第 3 節）"
            );
            return 0;
        }
        "rpm" => {
            println!(
                "rpm 安裝請以套件管理員回滾：\n  sudo rpm -U --oldpackage pulse-<版本>-<commit>-linux-<arch>.rpm"
            );
            return 0;
        }
        "tar" => {}
        other => {
            eprintln!(
                "錯誤 {}：無法判斷安裝方式（{other}），請手動回復",
                codes::W_UPGRADE_UNSUPPORTED
            );
            return 1;
        }
    }
    let dir = PathBuf::from(super::install::INSTALL_DIR);
    let prev = dir.join("pulse.prev");
    if !prev.is_file() {
        eprintln!(
            "錯誤：{} 不存在（只有遠端升級或 pulse upgrade 替換過的主機才有）",
            prev.display()
        );
        return 1;
    }
    if !yes {
        eprintln!(
            "錯誤 {}：這會以 {} 取代 {}/pulse 並重新啟動 pulse.service；確定要執行請加 --yes",
            codes::E_BAD_ARG_VALUE,
            prev.display(),
            dir.display()
        );
        return 2;
    }
    #[cfg(unix)]
    {
        // SAFETY: 只讀取目前行程的有效 uid
        if unsafe { libc::geteuid() } != 0 {
            eprintln!(
                "錯誤 {}：需要 root（sudo pulse upgrade rollback --yes）",
                codes::E_NEED_ADMIN
            );
            return 10;
        }
    }
    let current = dir.join("pulse");
    let tmp = dir.join("pulse.rollback-tmp");
    let r = std::fs::copy(&prev, &tmp).and_then(|_| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&tmp, &current)
    });
    if let Err(e) = r {
        eprintln!(
            "錯誤 {}：無法換回 pulse.prev：{e}",
            codes::E_UPGRADE_REPLACE
        );
        return 1;
    }
    let sys = super::upgrade::helper::RealSystem::default();
    use super::upgrade::helper::System;
    match sys.restart_service() {
        Ok(()) => {
            println!("已換回 {} 並重新啟動 pulse.service", prev.display());
            0
        }
        Err(e) => {
            eprintln!("已換回 pulse.prev，但無法重新啟動 pulse.service：{e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_config_show() {
        assert_eq!(
            parse_config(&s(&[
                "show",
                "--effective",
                "--sources",
                "--json",
                "-c",
                "/opt/pulse/pulse.toml"
            ]))
            .unwrap()
            .unwrap(),
            ConfigShowArgs {
                effective: true,
                sources: true,
                json: true,
                config: Some("/opt/pulse/pulse.toml".into())
            }
        );
        assert_eq!(parse_config(&s(&["show", "--help"])).unwrap(), None);
        assert!(parse_config(&s(&["edit"])).is_err());
        assert!(parse_config(&s(&["show", "--bogus"])).is_err());
        assert!(parse_config(&[]).is_err());
    }

    #[test]
    fn parses_upgrade_commands() {
        assert_eq!(
            parse_upgrade(&s(&["status", "--json"])).unwrap().unwrap(),
            UpgradeArgs::Status {
                json: true,
                config: None
            }
        );
        assert_eq!(
            parse_upgrade(&s(&[
                "apply",
                "--request",
                "/var/lib/pulse/upgrade/request.json"
            ]))
            .unwrap()
            .unwrap(),
            UpgradeArgs::Apply {
                request: PathBuf::from("/var/lib/pulse/upgrade/request.json")
            }
        );
        assert_eq!(
            parse_upgrade(&s(&["rollback", "--yes"])).unwrap().unwrap(),
            UpgradeArgs::Rollback {
                yes: true,
                config: None
            }
        );
        assert!(
            parse_upgrade(&s(&["apply"])).is_err(),
            "apply 必須有 --request"
        );
        assert!(parse_upgrade(&s(&["status", "--yes"])).is_err());
        assert!(parse_upgrade(&s(&["frobnicate"])).is_err());
        assert!(parse_upgrade(&[]).is_err());
        assert_eq!(parse_upgrade(&s(&["--help"])).unwrap(), None);
    }
}
