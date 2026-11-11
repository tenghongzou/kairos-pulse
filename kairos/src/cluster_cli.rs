//! `kairos cluster status|init|keys export|keys import|drain|step-down|forget`：叢集的維運命令
//! （`kairos/HA.md` 第 11.4、14.3 節）。
//!
//! 所有命令都直接讀寫資料庫，不需要 Kairos 在本機執行，也與節點的角色無關。
//!
//! - `status`：節點、leader 租約與金鑰狀態；`--json` 的 `schema` 為 `kairos.cluster.status/v1`。
//! - `init`：第一個節點建立叢集——金鑰不存在時建立、產生 `cluster_id`、把金鑰指紋寫入資料庫。
//! - `keys export`／`keys import`：以密碼加密分發所有需要一致的金鑰（格式 `kairos.keys-backup/v2`）。
//! - `drain <node_id>`：節點不再參與 leader 選舉；它是 leader 時正常關機並交還租約。
//! - `step-down`：讓目前的 leader 租約立即到期，備援節點在 2 秒內接手。
//! - `forget <node_id>`：刪除已停止節點的紀錄。

use chrono::Utc;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use crate::audit::Audit;
use crate::cluster::keys::LocalKeys;
use crate::cluster::store::{LEASE_LEADER, LeaseRow, NodeRow, role};
use crate::cluster::{CODE_COORDINATION, CODE_KEYS, META_CLUSTER_ID, META_CLUSTER_KEYS};
use crate::config::{AppConfig, StorageBackend, take_config_arg};
use crate::export_cli::PasswordSource;
use crate::hosts_cli::{ascii_json, use_unicode};
use crate::keys_backup::{self, Restored};
use crate::storage::fmt_time;
use crate::store::prelude::*;
use crate::token_cli::{Failure, db_failure, open, operator, table};

/// `kairos cluster --help`
pub const CLUSTER_USAGE: &str = "kairos cluster — 叢集（高可用）的維運命令（kairos/HA.md）

用法：
  kairos cluster status [--json]
  kairos cluster init
  kairos cluster keys export <檔案> (--password-file <檔案> | --password-stdin)
  kairos cluster keys import <檔案> (--password-file <檔案> | --password-stdin) [--overwrite]
  kairos cluster drain <node_id>
  kairos cluster step-down
  kairos cluster forget <node_id>

子命令：
  status      節點、leader 租約與金鑰狀態（直接讀資料庫，不需要 Kairos 在本機執行）
  init        第一個節點建立叢集：金鑰不存在時建立、產生 cluster_id、把金鑰指紋寫入資料庫。
              已建立過時只重新記錄這個節點的金鑰指紋（輪替金鑰之後使用）
  keys export 匯出所有需要一致的金鑰（alerts-secrets.key、console.key、audit-hmac.key、privacy.key 與
              簽發 CA 私鑰），以密碼加密（Argon2id＋AES-256-GCM，至少 12 個字元）；不覆寫既有檔案
  keys import 解開並寫入這個節點的資料目錄（0600）；已存在且相同時不變，不同時拒絕，除非 --overwrite
              （舊檔改名保留）。匯入後與資料庫中的指紋比對並顯示結果
  drain       節點不再參與 leader 選舉；它是 leader 時正常關機並交還租約（備援節點接手）。
              節點重新啟動後恢復
  step-down   讓目前的 leader 租約立即到期：leader 結束並由服務管理員重新啟動為備援，其他節點在
              2 秒內接手
  forget      刪除已停止（或失聯）節點的紀錄

選項：
  --config <path>    Kairos 的設定檔
  --json             status 輸出機器可讀的 JSON（純 ASCII；schema kairos.cluster.status/v1）
  --ascii            不使用 Unicode 符號

結束代碼：0 完成、1 資料庫或檔案錯誤、2 用法錯誤、3 設定錯誤（沒有啟用叢集模式、不是 PostgreSQL）、
9 status：叢集有嚴重問題（沒有 leader、金鑰不一致）、10 權限不足、11 輸出檔已存在或節點仍在執行";

const EXIT_GENERAL: u8 = 1;
const EXIT_CONFIG: u8 = 3;
const EXIT_CRITICAL: u8 = 9;
const EXIT_CONFLICT: u8 = 11;

pub const STATUS_SCHEMA: &str = "kairos.cluster.status/v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClusterAction {
    Help,
    Status,
    Init,
    KeysExport {
        dest: PathBuf,
        password: PasswordSource,
    },
    KeysImport {
        source: PathBuf,
        password: PasswordSource,
        overwrite: bool,
    },
    Drain(String),
    StepDown,
    Forget(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterCommand {
    pub action: ClusterAction,
    pub config_path: Option<PathBuf>,
    pub json: bool,
    pub ascii: bool,
}

pub fn parse_args(args: impl Iterator<Item = OsString>) -> Result<ClusterCommand, String> {
    let mut args = args.peekable();
    let mut cmd = ClusterCommand {
        action: ClusterAction::Help,
        config_path: None,
        json: false,
        ascii: false,
    };
    let sub = match args.next() {
        None => {
            return Err(
                "kairos cluster 需要子命令（status、init、keys、drain、step-down、forget）"
                    .to_string(),
            );
        }
        Some(a) if a == "-h" || a == "--help" => return Ok(cmd),
        Some(a) => a.to_string_lossy().into_owned(),
    };
    let keys_sub = if sub == "keys" {
        match args.next() {
            Some(a) if a == "export" || a == "import" => Some(a.to_string_lossy().into_owned()),
            Some(a) if a == "-h" || a == "--help" => return Ok(cmd),
            _ => return Err("kairos cluster keys 需要 export 或 import".to_string()),
        }
    } else {
        None
    };
    let mut positional: Vec<String> = Vec::new();
    let mut password = None;
    let mut overwrite = false;
    while let Some(arg) = args.next() {
        if let Some(path) = take_config_arg(&arg, &mut args)? {
            if cmd.config_path.replace(path).is_some() {
                return Err("--config 只能指定一次".to_string());
            }
            continue;
        }
        let a = arg
            .to_str()
            .ok_or_else(|| format!("無法辨識的參數: {}", arg.to_string_lossy()))?
            .to_string();
        match a.split('=').next().unwrap_or("") {
            "-h" | "--help" => {
                cmd.action = ClusterAction::Help;
                return Ok(cmd);
            }
            "--json" if sub == "status" => cmd.json = true,
            "--ascii" => cmd.ascii = true,
            "--password-stdin" if keys_sub.is_some() => {
                if password.replace(PasswordSource::Stdin).is_some() {
                    return Err("密碼來源只能指定一個".to_string());
                }
            }
            "--password-file" if keys_sub.is_some() => {
                let v = match a.strip_prefix("--password-file=") {
                    Some(v) => v.to_string(),
                    None => args
                        .next()
                        .and_then(|v| v.into_string().ok())
                        .ok_or("--password-file 需要指定檔案")?,
                };
                if password
                    .replace(PasswordSource::File(PathBuf::from(v)))
                    .is_some()
                {
                    return Err("密碼來源只能指定一個".to_string());
                }
            }
            "--overwrite" if keys_sub.as_deref() == Some("import") => overwrite = true,
            _ if a.starts_with('-') => return Err(format!("無法辨識的參數: {a}")),
            _ => positional.push(a),
        }
    }
    let mut one = |what: &str| -> Result<String, String> {
        match positional.len() {
            1 => Ok(positional.remove(0)),
            0 => Err(format!("kairos cluster {sub} 需要{what}")),
            _ => Err(format!("kairos cluster {sub} 只接受一個{what}")),
        }
    };
    let node_id = |id: String| -> Result<String, String> {
        if kairos_proto::is_server_node(&id) {
            Ok(id)
        } else {
            Err(format!(
                "「{id}」不是合法的 node_id（小寫英文字母、數字或 -）"
            ))
        }
    };
    cmd.action = match (sub.as_str(), keys_sub.as_deref()) {
        ("keys", Some(k)) => {
            let file = PathBuf::from(one("金鑰檔的路徑")?);
            let password = password
                .ok_or("kairos cluster keys 需要 --password-file <檔案> 或 --password-stdin")?;
            if k == "export" {
                ClusterAction::KeysExport {
                    dest: file,
                    password,
                }
            } else {
                ClusterAction::KeysImport {
                    source: file,
                    password,
                    overwrite,
                }
            }
        }
        ("drain", _) => ClusterAction::Drain(node_id(one(" node_id")?)?),
        ("forget", _) => ClusterAction::Forget(node_id(one(" node_id")?)?),
        (s @ ("status" | "init" | "step-down"), _) => {
            if !positional.is_empty() {
                return Err(format!("無法辨識的參數: {}", positional[0]));
            }
            match s {
                "status" => ClusterAction::Status,
                "init" => ClusterAction::Init,
                _ => ClusterAction::StepDown,
            }
        }
        (other, _) => {
            return Err(format!(
                "無法辨識的子命令: {other}（status、init、keys、drain、step-down、forget）"
            ));
        }
    };
    Ok(cmd)
}

/// 命令的輸出
#[derive(Debug, Default)]
struct Output {
    stdout: String,
    stderr: String,
    exit: u8,
}

fn marks(unicode: bool) -> (&'static str, &'static str, &'static str) {
    if unicode {
        ("✓", "!", "✗")
    } else {
        ("[OK]", "[WARN]", "[FAIL]")
    }
}

fn load_config(path: Option<&Path>) -> Result<AppConfig, Failure> {
    AppConfig::load(path).map_err(|e| Failure {
        code: Some(crate::diag::config_load_code(&e, path)),
        ..Failure::new(
            "設定檔無法載入",
            e.to_string(),
            vec!["修正設定檔後再執行，或以 --config 指定 Kairos 使用的設定檔。".to_string()],
            EXIT_CONFIG,
        )
    })
}

/// 需要叢集模式的命令：設定必須是 `cluster.enabled = true` 與 PostgreSQL
fn require_cluster(config: &AppConfig) -> Result<(), Failure> {
    if config.cluster.enabled && config.storage.backend == StorageBackend::Postgres {
        return Ok(());
    }
    Err(Failure::new(
        "這個命令需要叢集模式",
        "設定檔沒有啟用叢集模式（cluster.enabled = true），或 storage.backend 不是 \"postgres\"。",
        vec![
            "依 INSTALL 的「叢集部署」設定 [cluster] 與 [storage.postgres] 後再執行。".to_string(),
        ],
        EXIT_CONFIG,
    ))
}

fn open_db(cmd: &ClusterCommand) -> Result<DbHandle, Failure> {
    open(cmd.config_path.as_deref(), true)?.ok_or_else(|| {
        Failure::new(
            "找不到資料庫",
            "資料庫還不存在。",
            vec!["確認 --config 指向 Kairos 使用的設定檔。".to_string()],
            EXIT_GENERAL,
        )
    })
}

fn uuid_v4() -> Result<String, Failure> {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| db_failure("無法取得隨機數（作業系統的 CSPRNG）"))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    ))
}

fn audit_cli(db: &dyn Store, action: &'static str, object: Option<&str>, details: Value) {
    let actor = operator();
    let r = db.audit(
        Utc::now().timestamp_millis(),
        &Audit {
            actor: &actor,
            category: "cluster",
            action,
            object,
            result: "ok",
            details: Some(details),
            ..Default::default()
        },
    );
    if let Err(e) = r {
        eprintln!("警告：無法寫入稽核紀錄（{action}）：{e}");
    }
}

/// 節點是否失聯（心跳超過 `node_lost_secs`）
fn node_lost(n: &NodeRow, config: &AppConfig) -> bool {
    n.role == role::LOST
        || (n.role != role::STOPPED
            && n.heartbeat_age_ms
                > i64::try_from(config.cluster.node_lost_after().as_millis()).unwrap_or(i64::MAX))
}

/// `status` 的資料（也用於主控台 API `GET /api/v1/cluster` 與系統健康）
pub fn status_value(
    config: &AppConfig,
    cluster_id: Option<&str>,
    lease: Option<&LeaseRow>,
    nodes: &[NodeRow],
) -> Value {
    let live = lease.filter(|l| l.is_live());
    let leader_hash = live
        .and_then(|l| nodes.iter().find(|n| n.node_id == l.holder))
        .map(|n| (n.config_hash.clone(), n.keys_hash.clone()));
    let nodes_json: Vec<Value> = nodes
        .iter()
        .map(|n| {
            let info: Value = n
                .info
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null);
            let lost = node_lost(n, config);
            json!({
                "node_id": n.node_id,
                "hostname": n.hostname,
                "role": if lost && n.role != role::LOST { role::LOST } else { n.role.as_str() },
                "version": n.version,
                "schema_version": n.schema_version,
                "advertise": n.advertise,
                "started_at": fmt_time(chrono::DateTime::from_timestamp_millis(n.started_at).unwrap_or_default()),
                "heartbeat_age_ms": n.heartbeat_age_ms,
                "connections": info.get("connections").cloned().unwrap_or(Value::Null),
                "config_drift": leader_hash.as_ref().is_some_and(|(c, _)| *c != n.config_hash),
                "keys_ok": info.get("keys_ok").and_then(Value::as_bool).unwrap_or(false)
                    && leader_hash.as_ref().is_none_or(|(_, k)| *k == n.keys_hash),
                "key_problems": info.get("key_problems").cloned().unwrap_or(json!([])),
            })
        })
        .collect();
    json!({
        "enabled": config.cluster.enabled,
        "mode": config.cluster.mode,
        "cluster_id": cluster_id,
        "leader": live.map(|l| json!({
            "node_id": l.holder,
            "epoch": l.epoch,
            "since": fmt_time(chrono::DateTime::from_timestamp_millis(l.acquired_at).unwrap_or_default()),
            "expires_in_ms": l.expires_at - l.now,
        })),
        "last_leader": lease.filter(|l| !l.is_live()).map(|l| json!({
            "node_id": l.holder,
            "epoch": l.epoch,
            "expired_ms_ago": l.now - l.expires_at,
        })),
        "nodes": nodes_json,
    })
}

/// 叢集的問題：(嚴重, 訊息)
pub fn status_problems(config: &AppConfig, status: &Value) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    if !config.cluster.enabled {
        return out;
    }
    if status["cluster_id"].is_null() {
        out.push((
            true,
            format!(
                "{CODE_KEYS}：尚未執行 kairos cluster init（資料庫中沒有 cluster_id 與金鑰指紋）"
            ),
        ));
    }
    if status["leader"].is_null() {
        out.push((
            true,
            format!("{CODE_COORDINATION}：目前沒有任何節點持有 leader 租約"),
        ));
    }
    let empty = Vec::new();
    let nodes = status["nodes"].as_array().unwrap_or(&empty);
    let mut alive = 0;
    for n in nodes {
        let id = n["node_id"].as_str().unwrap_or("?");
        match n["role"].as_str().unwrap_or("") {
            role::LOST => out.push((
                false,
                format!("{}：節點 {id} 失聯", crate::cluster::CODE_NODE_LOST),
            )),
            role::STOPPED => {}
            _ => {
                alive += 1;
                if n["keys_ok"] == json!(false) {
                    out.push((
                        true,
                        format!("{CODE_KEYS}：節點 {id} 的金鑰與叢集不一致或缺少"),
                    ));
                }
                if n["config_drift"] == json!(true) {
                    out.push((
                        false,
                        format!(
                            "{}：節點 {id} 需要一致的設定與 leader 不同",
                            crate::cluster::CODE_CONFIG_DRIFT
                        ),
                    ));
                }
            }
        }
    }
    if alive == 1 {
        out.push((false, "只有一個節點在執行（沒有備援）".to_string()));
    }
    out
}

fn fmt_age(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0..=119 => format!("{s} 秒前"),
        120..=7199 => format!("{} 分鐘前", s / 60),
        7200..=172_799 => format!("{} 小時前", s / 3600),
        _ => format!("{} 天前", s / 86_400),
    }
}

fn status(cmd: &ClusterCommand, unicode: bool) -> Result<Output, Failure> {
    let config = load_config(cmd.config_path.as_deref())?;
    let db = open_db(cmd)?;
    let cluster_id = db.get_meta(META_CLUSTER_ID).map_err(db_failure)?;
    let lease = db.cluster_lease(LEASE_LEADER, None).map_err(db_failure)?;
    let nodes = db.cluster_nodes(None).map_err(db_failure)?;
    let value = status_value(&config, cluster_id.as_deref(), lease.as_ref(), &nodes);
    let problems = status_problems(&config, &value);
    let critical = problems.iter().any(|(c, _)| *c);
    let exit = if critical { EXIT_CRITICAL } else { 0 };
    if cmd.json {
        let warnings: Vec<&String> = problems.iter().map(|(_, m)| m).collect();
        return Ok(Output {
            stdout: ascii_json(&json!({
                "schema": STATUS_SCHEMA,
                "ok": !critical,
                "generated_at": fmt_time(Utc::now()),
                "data": value,
                "warnings": warnings,
                "error": null,
            })) + "\n",
            exit,
            ..Output::default()
        });
    }
    let (ok, warn, fail) = marks(unicode);
    let mut out = String::new();
    if !config.cluster.enabled {
        out.push_str("叢集模式：未啟用（cluster.enabled = false；單機）\n");
    } else {
        out.push_str(&format!(
            "叢集模式：已啟用（{}）\n叢集 ID：{}\n",
            config.cluster.mode,
            cluster_id
                .as_deref()
                .unwrap_or("（尚未執行 kairos cluster init）")
        ));
    }
    match (&value["leader"], &value["last_leader"]) {
        (l, _) if !l.is_null() => out.push_str(&format!(
            "leader：{}（epoch {}，自 {}；租約還有 {:.1} 秒）\n",
            l["node_id"].as_str().unwrap_or("?"),
            l["epoch"],
            l["since"].as_str().unwrap_or("?"),
            l["expires_in_ms"].as_f64().unwrap_or(0.0) / 1000.0
        )),
        (_, l) if !l.is_null() => out.push_str(&format!(
            "leader：沒有（上一個是 {}，epoch {}，租約已到期 {}）\n",
            l["node_id"].as_str().unwrap_or("?"),
            l["epoch"],
            fmt_age(l["expired_ms_ago"].as_i64().unwrap_or(0))
        )),
        _ => out.push_str("leader：沒有（還沒有節點取得過租約）\n"),
    }
    if nodes.is_empty() {
        out.push_str("\n沒有節點紀錄。\n");
    } else {
        let mut rows = vec![
            [
                "節點",
                "角色",
                "版本",
                "心跳",
                "連線數",
                "設定",
                "金鑰",
                "位址",
            ]
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ];
        let empty = Vec::new();
        for n in value["nodes"].as_array().unwrap_or(&empty) {
            let stopped = n["role"] == json!(role::STOPPED);
            rows.push(vec![
                n["node_id"].as_str().unwrap_or("?").to_string(),
                n["role"].as_str().unwrap_or("?").to_string(),
                n["version"].as_str().unwrap_or("?").to_string(),
                fmt_age(n["heartbeat_age_ms"].as_i64().unwrap_or(0)),
                n["connections"]
                    .as_u64()
                    .map_or("-".to_string(), |c| c.to_string()),
                if stopped {
                    "-"
                } else if n["config_drift"] == json!(true) {
                    "不一致"
                } else {
                    "一致"
                }
                .to_string(),
                if stopped {
                    "-"
                } else if n["keys_ok"] == json!(true) {
                    "一致"
                } else {
                    "不一致"
                }
                .to_string(),
                n["advertise"].as_str().unwrap_or("-").to_string(),
            ]);
        }
        out.push('\n');
        out.push_str(&table(&rows));
    }
    out.push('\n');
    if problems.is_empty() {
        if config.cluster.enabled {
            out.push_str(&format!("{ok} 叢集狀態正常\n"));
        }
    } else {
        for (critical, m) in &problems {
            out.push_str(&format!("{} {m}\n", if *critical { fail } else { warn }));
        }
    }
    Ok(Output {
        stdout: out,
        exit,
        ..Output::default()
    })
}

fn init(cmd: &ClusterCommand, unicode: bool) -> Result<Output, Failure> {
    let config = load_config(cmd.config_path.as_deref())?;
    require_cluster(&config)?;
    let data_dir = PathBuf::from(&config.storage.data_dir);
    std::fs::create_dir_all(&data_dir).map_err(|e| {
        Failure::new(
            "無法建立資料目錄",
            format!("{}: {e}", data_dir.display()),
            vec![],
            EXIT_GENERAL,
        )
    })?;
    let db = open_db(cmd)?;
    let (ok, warn, _) = marks(unicode);
    let mut out = String::new();
    let existing_id = db.get_meta(META_CLUSTER_ID).map_err(db_failure)?;
    let existing_keys = db.get_meta(META_CLUSTER_KEYS).map_err(db_failure)?;
    let cas = db.list_cas().map_err(db_failure)?;

    // 已建立過的叢集：這個節點的金鑰必須與記錄的一致才重新記錄（避免第二個節點誤執行 init 蓋掉指紋）
    if let Some(meta) = existing_keys.as_deref() {
        let local = LocalKeys::read(&data_dir);
        let recorded: std::collections::BTreeMap<String, String> =
            serde_json::from_str::<Value>(meta)
                .ok()
                .and_then(|v| serde_json::from_value(v.get("keys")?.clone()).ok())
                .unwrap_or_default();
        let conflicting = recorded
            .iter()
            .any(|(name, fp)| local.symmetric.get(name).is_none_or(|l| l != fp));
        if conflicting {
            return Err(Failure {
                code: Some(CODE_KEYS),
                ..Failure::new(
                    "這個資料庫已經建立過叢集，而且這個節點的金鑰與叢集記錄的不同",
                    local.compare(Some(meta), &cas).join("；"),
                    vec![
                        "這是要加入既有叢集的節點：從第一個節點執行 kairos cluster keys export，再在這裡執行 kairos cluster keys import。".to_string(),
                        "不要在第二個節點執行 kairos cluster init。".to_string(),
                    ],
                    EXIT_CONFLICT,
                )
            });
        }
    }
    let created = crate::cluster::keys::create_missing(&data_dir).map_err(|e| {
        Failure::new(
            "無法建立金鑰檔",
            e.to_string(),
            vec!["以 Kairos 的服務帳號執行（sudo -u kairos …），並確認資料目錄可寫。".to_string()],
            if e.kind() == ErrorKind::PermissionDenied {
                10
            } else {
                EXIT_GENERAL
            },
        )
    })?;
    let local = LocalKeys::read(&data_dir);
    let cluster_id = match existing_id {
        Some(id) => id,
        None => uuid_v4()?,
    };
    let meta = local.to_meta();
    {
        let tx = db.begin(TxMode::Immediate).map_err(db_failure)?;
        tx.put_meta(META_CLUSTER_ID, &cluster_id)
            .map_err(db_failure)?;
        tx.put_meta(META_CLUSTER_KEYS, &meta).map_err(db_failure)?;
        tx.commit().map_err(db_failure)?;
    }
    audit_cli(
        db.as_ref(),
        "cluster.init",
        Some(&cluster_id),
        json!({ "keys_created": created, "keys": local.symmetric.keys().collect::<Vec<_>>() }),
    );
    out.push_str(&format!("{ok} 叢集已建立：cluster_id {cluster_id}\n"));
    if created.is_empty() {
        out.push_str("  金鑰檔都已存在，沒有建立新的。\n");
    } else {
        out.push_str(&format!("  已建立金鑰檔：{}\n", created.join("、")));
    }
    out.push_str(&format!(
        "  已把 {} 把對稱金鑰的指紋寫入資料庫（指紋雜湊 {}）。\n",
        local.symmetric.len(),
        local.hash()
    ));
    let ca_problems: Vec<String> = local
        .compare(Some(&meta), &cas)
        .into_iter()
        .filter(|p| p.contains("簽發 CA"))
        .collect();
    for p in &ca_problems {
        out.push_str(&format!("{warn} {p}\n"));
    }
    out.push_str(
        "\n下一步：\n  1. 啟動這個節點（systemctl start kairos）：它會取得 leader 租約。\n  2. 其他節點：kairos cluster keys export <檔案> --password-stdin（在這裡），把檔案帶到其他節點後\n     kairos cluster keys import <檔案> --password-stdin，再啟動。\n  3. kairos cluster status 應顯示一個 leader 與其他備援節點。\n",
    );
    Ok(Output {
        stdout: out,
        ..Output::default()
    })
}

fn read_password(src: &PasswordSource) -> Result<keys_backup::Password, Failure> {
    match src {
        PasswordSource::File(p) => keys_backup::Password::from_file(p),
        PasswordSource::Stdin => keys_backup::Password::from_reader(std::io::stdin().lock()),
    }
    .map_err(|e| {
        Failure::new(
            "密碼無法使用",
            e,
            vec![
                "密碼至少 12 個字元；以 --password-file <檔案> 或 --password-stdin 提供。"
                    .to_string(),
            ],
            2,
        )
    })
}

fn keys_export(
    cmd: &ClusterCommand,
    dest: &Path,
    password: &PasswordSource,
    unicode: bool,
) -> Result<Output, Failure> {
    let config = load_config(cmd.config_path.as_deref())?;
    let data_dir = PathBuf::from(&config.storage.data_dir);
    if dest.exists() {
        return Err(Failure::new(
            "輸出檔已存在",
            format!("{} 已存在；不覆寫。", dest.display()),
            vec!["指定另一個檔名，或先移走既有的檔案。".to_string()],
            EXIT_CONFLICT,
        ));
    }
    let password = read_password(password)?;
    let (files, missing) = keys_backup::read_cluster_keys(&data_dir).map_err(|e| {
        Failure::new(
            "無法讀取金鑰檔",
            e.to_string(),
            vec!["以 Kairos 的服務帳號執行（金鑰檔是 0600）。".to_string()],
            if e.kind() == ErrorKind::PermissionDenied {
                10
            } else {
                EXIT_GENERAL
            },
        )
    })?;
    if files.is_empty() {
        return Err(Failure {
            code: Some(CODE_KEYS),
            ..Failure::new(
                "資料目錄中沒有任何金鑰檔",
                format!("{} 中沒有可以匯出的金鑰。", data_dir.display()),
                vec!["在第一個節點先執行 kairos cluster init。".to_string()],
                EXIT_GENERAL,
            )
        });
    }
    let text = keys_backup::seal_as(
        keys_backup::FORMAT_V2,
        &files,
        &password,
        keys_backup::DEFAULT_KDF,
    )
    .map_err(|e| Failure::new("無法加密金鑰", e, vec![], EXIT_GENERAL))?;
    keys_backup::write_new_private(dest, text.as_bytes()).map_err(|e| {
        Failure::new(
            "無法寫入輸出檔",
            e.to_string(),
            vec![],
            if e.kind() == ErrorKind::AlreadyExists {
                EXIT_CONFLICT
            } else {
                EXIT_GENERAL
            },
        )
    })?;
    let (ok, warn, _) = marks(unicode);
    let mut out = format!(
        "{ok} 已匯出 {} 個金鑰檔到 {}（{}，以密碼加密；權限 0600）\n",
        files.len(),
        dest.display(),
        keys_backup::FORMAT_V2
    );
    for (name, _) in &files {
        out.push_str(&format!("  {name}\n"));
    }
    for m in missing {
        out.push_str(&format!("{warn} 沒有 {m}（這個節點還沒有建立；未匯出）\n"));
    }
    out.push_str(
        "\n這個檔案加上密碼就等於所有金鑰：以安全的方式帶到其他節點，匯入後刪除；密碼另外傳遞。\n",
    );
    Ok(Output {
        stdout: out,
        ..Output::default()
    })
}

fn keys_import(
    cmd: &ClusterCommand,
    source: &Path,
    password: &PasswordSource,
    overwrite: bool,
    unicode: bool,
) -> Result<Output, Failure> {
    let config = load_config(cmd.config_path.as_deref())?;
    let data_dir = PathBuf::from(&config.storage.data_dir);
    let text = std::fs::read_to_string(source).map_err(|e| {
        Failure::new(
            "無法讀取金鑰檔",
            format!("{}: {e}", source.display()),
            vec![],
            EXIT_GENERAL,
        )
    })?;
    let password = read_password(password)?;
    let files = keys_backup::open(&text, &password).map_err(|e| Failure {
        code: Some(keys_backup::CODE_KEYS_BACKUP),
        ..Failure::new(
            "無法解開金鑰檔",
            e,
            vec!["確認密碼，以及檔案是 kairos cluster keys export 產生的。".to_string()],
            EXIT_GENERAL,
        )
    })?;
    std::fs::create_dir_all(&data_dir).map_err(|e| {
        Failure::new(
            "無法建立資料目錄",
            format!("{}: {e}", data_dir.display()),
            vec![],
            EXIT_GENERAL,
        )
    })?;
    let restored = keys_backup::restore(&data_dir, &files, overwrite).map_err(|e| {
        let exit = match e.kind() {
            ErrorKind::AlreadyExists => EXIT_CONFLICT,
            ErrorKind::PermissionDenied => 10,
            _ => EXIT_GENERAL,
        };
        Failure {
            code: Some(CODE_KEYS),
            ..Failure::new(
                "無法寫入金鑰檔",
                e.to_string(),
                vec![
                    "這個節點已有不同的金鑰：確認它不是另一個叢集的節點後加 --overwrite（舊檔會改名保留）。".to_string(),
                ],
                exit,
            )
        }
    })?;
    let (ok, warn, fail) = marks(unicode);
    let mut out = String::new();
    for (name, r) in &restored {
        out.push_str(&match r {
            Restored::Written => format!("{ok} {name}：已寫入\n"),
            Restored::Unchanged => format!("{ok} {name}：與現有的相同，未變更\n"),
            Restored::Replaced(kept) => {
                format!("{warn} {name}：已取代（舊檔保留在 {}）\n", kept.display())
            }
        });
    }
    // 匯入後立即與資料庫中的指紋比對（連不上資料庫時只提示）
    let mut exit = 0;
    match open(cmd.config_path.as_deref(), false) {
        Ok(Some(db)) => {
            let meta = db.get_meta(META_CLUSTER_KEYS).map_err(db_failure)?;
            let cas = db.list_cas().map_err(db_failure)?;
            let local = LocalKeys::read(&data_dir);
            let problems = local.compare(meta.as_deref(), &cas);
            if problems.is_empty() {
                out.push_str(&format!(
                    "\n{ok} 這個節點的金鑰與叢集一致（指紋雜湊 {}）；可以啟動 Kairos。\n",
                    local.hash()
                ));
            } else {
                exit = EXIT_CRITICAL;
                out.push_str(&format!(
                    "\n{fail} {CODE_KEYS}：這個節點的金鑰與叢集仍不一致：\n"
                ));
                for p in problems {
                    out.push_str(&format!("    {p}\n"));
                }
            }
        }
        Ok(None) | Err(_) => out.push_str(&format!(
            "\n{warn} 無法連線到資料庫，沒有與叢集記錄的指紋比對；啟動後以 kairos cluster status 確認。\n"
        )),
    }
    Ok(Output {
        stdout: out,
        exit,
        ..Output::default()
    })
}

fn drain(cmd: &ClusterCommand, node_id: &str, unicode: bool) -> Result<Output, Failure> {
    let config = load_config(cmd.config_path.as_deref())?;
    require_cluster(&config)?;
    let db = open_db(cmd)?;
    let node = db
        .cluster_node(node_id, None)
        .map_err(db_failure)?
        .ok_or_else(|| {
            Failure::new(
                "找不到這個節點",
                format!("叢集中沒有 node_id 為 {node_id} 的節點。"),
                vec!["以 kairos cluster status 查看節點清單。".to_string()],
                EXIT_GENERAL,
            )
        })?;
    let (ok, _, _) = marks(unicode);
    if matches!(node.role.as_str(), role::STOPPED | role::LOST) {
        return Ok(Output {
            stdout: format!(
                "{ok} 節點 {node_id} 已經停止（{}），不需要排空\n",
                node.role
            ),
            ..Output::default()
        });
    }
    db.cluster_node_set_role(node_id, role::DRAINING)
        .map_err(db_failure)?;
    audit_cli(
        db.as_ref(),
        "cluster.drain",
        Some(node_id),
        json!({ "role": node.role }),
    );
    let was_leader = node.role == role::LEADER;
    Ok(Output {
        stdout: format!(
            "{ok} 已要求節點 {node_id} 排空（KRS-I8507）：{}\n  節點在下一次心跳（{} 秒內）生效；重新啟動後恢復參與選舉。\n",
            if was_leader {
                "它是 leader，會正常關機並交還租約，由備援節點接手（systemd 不會自動重新啟動正常結束的服務）"
            } else {
                "它不再參與 leader 選舉"
            },
            config.cluster.heartbeat_secs
        ),
        ..Output::default()
    })
}

fn step_down(cmd: &ClusterCommand, unicode: bool) -> Result<Output, Failure> {
    let config = load_config(cmd.config_path.as_deref())?;
    require_cluster(&config)?;
    let db = open_db(cmd)?;
    let vacated = db
        .cluster_lease_vacate(LEASE_LEADER, None)
        .map_err(db_failure)?;
    let (ok, warn, _) = marks(unicode);
    let Some(l) = vacated else {
        return Ok(Output {
            stdout: format!("{warn} 目前沒有節點持有 leader 租約，沒有東西可以交還\n"),
            ..Output::default()
        });
    };
    audit_cli(
        db.as_ref(),
        "cluster.step_down",
        Some(&l.holder),
        json!({ "epoch": l.epoch }),
    );
    Ok(Output {
        stdout: format!(
            "{ok} 已讓 {} 的 leader 租約（epoch {}）到期：它會結束並由服務管理員重新啟動為備援，其他節點在約 2 秒內接手。\n  以 kairos cluster status 確認新的 leader。\n",
            l.holder, l.epoch
        ),
        ..Output::default()
    })
}

fn forget(cmd: &ClusterCommand, node_id: &str, unicode: bool) -> Result<Output, Failure> {
    let config = load_config(cmd.config_path.as_deref())?;
    let db = open_db(cmd)?;
    let node = db
        .cluster_node(node_id, None)
        .map_err(db_failure)?
        .ok_or_else(|| {
            Failure::new(
                "找不到這個節點",
                format!("叢集中沒有 node_id 為 {node_id} 的節點。"),
                vec!["以 kairos cluster status 查看節點清單。".to_string()],
                EXIT_GENERAL,
            )
        })?;
    if !node_lost(&node, &config) && node.role != role::STOPPED {
        return Err(Failure::new(
            "節點仍在執行",
            format!(
                "節點 {node_id} 的角色是 {}，{} 毫秒前還有心跳。",
                node.role, node.heartbeat_age_ms
            ),
            vec!["先停止這個節點（systemctl stop kairos），再執行 forget。".to_string()],
            EXIT_CONFLICT,
        ));
    }
    db.cluster_node_forget(node_id).map_err(db_failure)?;
    audit_cli(
        db.as_ref(),
        "cluster.forget",
        Some(node_id),
        json!({ "role": node.role }),
    );
    let (ok, _, _) = marks(unicode);
    Ok(Output {
        stdout: format!("{ok} 已刪除節點 {node_id} 的紀錄\n"),
        ..Output::default()
    })
}

pub fn run(cmd: ClusterCommand) -> u8 {
    let unicode = !cmd.json && use_unicode(cmd.ascii);
    let result = match &cmd.action {
        ClusterAction::Help => Ok(Output {
            stdout: format!("{CLUSTER_USAGE}\n"),
            ..Output::default()
        }),
        ClusterAction::Status => status(&cmd, unicode),
        ClusterAction::Init => init(&cmd, unicode),
        ClusterAction::KeysExport { dest, password } => keys_export(&cmd, dest, password, unicode),
        ClusterAction::KeysImport {
            source,
            password,
            overwrite,
        } => keys_import(&cmd, source, password, *overwrite, unicode),
        ClusterAction::Drain(id) => drain(&cmd, id, unicode),
        ClusterAction::StepDown => step_down(&cmd, unicode),
        ClusterAction::Forget(id) => forget(&cmd, id, unicode),
    };
    let out = result.unwrap_or_else(|f| {
        if cmd.json {
            Output {
                stdout: f.json(STATUS_SCHEMA, Utc::now()),
                exit: f.exit,
                ..Output::default()
            }
        } else {
            Output {
                stderr: f.human(unicode),
                exit: f.exit,
                ..Output::default()
            }
        }
    });
    let mut stdout = std::io::stdout().lock();
    let wrote = stdout
        .write_all(out.stdout.as_bytes())
        .and_then(|_| stdout.flush());
    eprint!("{}", out.stderr);
    match wrote {
        Err(e) if e.kind() != ErrorKind::BrokenPipe => out.exit.max(1),
        _ => out.exit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<ClusterCommand, String> {
        parse_args(args.iter().map(OsString::from))
    }

    #[test]
    fn subcommands_are_parsed() {
        assert_eq!(
            parse(&["status", "--json"]).unwrap().action,
            ClusterAction::Status
        );
        assert!(parse(&["status", "--json"]).unwrap().json);
        assert_eq!(parse(&["init"]).unwrap().action, ClusterAction::Init);
        assert_eq!(
            parse(&["step-down"]).unwrap().action,
            ClusterAction::StepDown
        );
        assert_eq!(
            parse(&["drain", "kairos-b"]).unwrap().action,
            ClusterAction::Drain("kairos-b".into())
        );
        assert_eq!(
            parse(&["forget", "kairos-b", "--config", "/etc/k.toml"]).unwrap(),
            ClusterCommand {
                action: ClusterAction::Forget("kairos-b".into()),
                config_path: Some(PathBuf::from("/etc/k.toml")),
                json: false,
                ascii: false,
            }
        );
        assert_eq!(
            parse(&["keys", "export", "/root/k.keys", "--password-stdin"])
                .unwrap()
                .action,
            ClusterAction::KeysExport {
                dest: PathBuf::from("/root/k.keys"),
                password: PasswordSource::Stdin,
            }
        );
        assert_eq!(
            parse(&[
                "keys",
                "import",
                "k.keys",
                "--password-file=pw",
                "--overwrite"
            ])
            .unwrap()
            .action,
            ClusterAction::KeysImport {
                source: PathBuf::from("k.keys"),
                password: PasswordSource::File(PathBuf::from("pw")),
                overwrite: true,
            }
        );
        assert_eq!(parse(&["--help"]).unwrap().action, ClusterAction::Help);
    }

    #[test]
    fn usage_errors_are_reported() {
        for (args, what) in [
            (&[][..], "需要子命令"),
            (&["bogus"][..], "無法辨識的子命令"),
            (&["drain"][..], "需要"),
            (&["drain", "Bad_Node"][..], "不是合法的 node_id"),
            (&["keys"][..], "export 或 import"),
            (&["keys", "export", "f"][..], "--password"),
            (
                &["keys", "export", "f", "--overwrite", "--password-stdin"][..],
                "無法辨識",
            ),
            (&["status", "extra"][..], "無法辨識"),
            (&["init", "--json"][..], "無法辨識"),
        ] {
            let e = parse(args).unwrap_err();
            assert!(e.contains(what), "{args:?}: {e}");
        }
    }

    fn node(id: &str, role: &str, age: i64, config: &str, info: &str) -> NodeRow {
        NodeRow {
            node_id: id.into(),
            hostname: id.into(),
            version: "kairos 0.10.0".into(),
            schema_version: 10,
            role: role.into(),
            config_hash: config.into(),
            keys_hash: "k".into(),
            advertise: None,
            started_at: 1_700_000_000_000,
            heartbeat_at: 1_700_000_000_000,
            info: Some(info.into()),
            heartbeat_age_ms: age,
        }
    }

    fn cluster_config() -> AppConfig {
        let mut c = AppConfig::default();
        c.cluster.enabled = true;
        c
    }

    #[test]
    fn status_reports_leader_nodes_and_problems() {
        let config = cluster_config();
        let lease = LeaseRow {
            holder: "kairos-a".into(),
            epoch: 3,
            acquired_at: 1_700_000_000_000,
            expires_at: 1_700_000_010_000,
            now: 1_700_000_001_000,
        };
        let good = r#"{"connections":12,"keys_ok":true}"#;
        let nodes = vec![
            node("kairos-a", "leader", 800, "c1", good),
            node(
                "kairos-b",
                "standby",
                1_200,
                "c1",
                r#"{"connections":0,"keys_ok":true}"#,
            ),
        ];
        let v = status_value(&config, Some("id"), Some(&lease), &nodes);
        assert_eq!(v["leader"]["node_id"], "kairos-a");
        assert_eq!(v["leader"]["epoch"], 3);
        assert_eq!(v["leader"]["expires_in_ms"], 9_000);
        assert_eq!(v["nodes"][0]["connections"], 12);
        assert!(status_problems(&config, &v).is_empty());

        // 設定不一致（警告）、金鑰不一致（嚴重）、節點失聯（警告）
        let nodes = vec![
            node("kairos-a", "leader", 800, "c1", good),
            node(
                "kairos-b",
                "standby",
                1_200,
                "c2",
                r#"{"keys_ok":false,"key_problems":["缺少 console.key"]}"#,
            ),
            node("kairos-c", "standby", 60_000, "c1", good),
        ];
        let v = status_value(&config, Some("id"), Some(&lease), &nodes);
        assert_eq!(v["nodes"][2]["role"], "lost");
        let p = status_problems(&config, &v);
        assert!(
            p.iter()
                .any(|(c, m)| *c && m.contains("KRS-E8504") && m.contains("kairos-b")),
            "{p:?}"
        );
        assert!(
            p.iter().any(|(c, m)| !*c && m.contains("KRS-W8505")),
            "{p:?}"
        );
        assert!(
            p.iter()
                .any(|(c, m)| !*c && m.contains("KRS-W8506") && m.contains("kairos-c")),
            "{p:?}"
        );

        // 租約已到期：沒有 leader（嚴重）；只剩一個節點（警告）
        let expired = LeaseRow {
            now: 1_700_000_020_000,
            ..lease
        };
        let nodes = vec![
            node("kairos-a", "stopped", 20_000, "c1", good),
            node("kairos-b", "standby", 1_200, "c1", good),
        ];
        let v = status_value(&config, Some("id"), Some(&expired), &nodes);
        assert!(v["leader"].is_null());
        assert_eq!(v["last_leader"]["node_id"], "kairos-a");
        let p = status_problems(&config, &v);
        assert!(
            p.iter().any(|(c, m)| *c && m.contains("KRS-E8503")),
            "{p:?}"
        );
        assert!(
            p.iter().any(|(c, m)| !*c && m.contains("沒有備援")),
            "{p:?}"
        );

        // 單機：沒有問題
        assert!(status_problems(&AppConfig::default(), &v).is_empty());
    }

    #[test]
    fn uuid_is_version_4() {
        let id = uuid_v4().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(id.as_bytes()[14], b'4');
        assert!(matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
        assert_ne!(id, uuid_v4().unwrap());
    }
}
