//! `kairos pki init`、`kairos pki sign`、`kairos pki gencrl`、`kairos pki revocations export`
//! （`kairos/PKI.md` 第 15 節；實作在 [`crate::pki::ca`]）。
//!
//! - `init`：以服務帳號執行（寫入資料庫與 `data/pki/`）；只能執行一次，任何要寫出的檔案已存在或資料庫已有
//!   簽發 CA 時拒絕（結束代碼 11）。成功後寫一筆稽核（`pki` / `pki.init`）。
//! - `sign`、離線的 `gencrl`：在 CA 主機上執行，不讀 Kairos 的設定與資料庫；CA 私鑰必須是 0600，
//!   輸出檔已存在時不覆寫，簽發前先附加一筆紀錄到 CA 私鑰旁的 `kairos-ca.log`。
//! - 線上模式的 `gencrl`（不帶 `--ca-key`）：以常駐行程與主控台相同的 `PkiManager::regenerate_crl` 產生，
//!   寫一筆稽核（`pki` / `crl.generate`），執行中的 Kairos 在下一次 TLS 重載（`server.reload_interval_secs`）載入。

use serde_json::json;
use std::path::{Path, PathBuf};

use crate::enroll::store;
use crate::enroll_cli::{
    Cmd, EXIT_CERT, EXIT_CONFLICT, EXIT_GENERAL, Env, db, int_opt, load_config, mark, ok_json,
    read_file, usage_err, write_new_file,
};
use crate::pki::ca::{self, CaKey, InitError, InitPlan, ServerName};
use crate::pki::x509::{parse_cert, pem_decode_all, pem_encode};
use crate::token_cli::{Failure, Output, db_failure, human_time, operator};

/// 根 CA 私鑰仍在伺服器上（`pki init` 之後提醒移到離線保存）〔暫定〕
pub const CODE_ROOT_KEY_ON_SERVER: &str = "KRS-W1306";
/// `pki sign`／`gencrl` 的 CA 憑證或私鑰無法使用（權限過寬、不成對、不是 CA、已過期）〔暫定〕
pub const CODE_CA_UNUSABLE: &str = "KRS-E1307";

fn ca_failure(msg: impl Into<String>) -> Failure {
    Failure {
        code: Some(CODE_CA_UNUSABLE),
        ..Failure::new(
            "CA 憑證或私鑰無法使用",
            msg,
            vec![
                "確認 --ca-cert 與 --ca-key 是同一個 CA、私鑰為未加密的 PKCS#8 PEM 且權限為 0600。"
                    .into(),
            ],
            EXIT_CERT,
        )
    }
}

fn general(title: &str, msg: impl Into<String>) -> Failure {
    Failure::new(title, msg, Vec::new(), EXIT_GENERAL)
}

fn days_opt(cmd: &Cmd, default: i64) -> Result<i64, Failure> {
    Ok(int_opt(cmd, "--days")?.unwrap_or(default))
}

/// 檢查輸出檔不存在（不覆寫）
fn refuse_existing(paths: &[&str]) -> Result<(), Failure> {
    for p in paths {
        if Path::new(p).exists() {
            return Err(Failure::new(
                format!("{p} 已存在"),
                "輸出檔已存在時不覆寫",
                vec!["換一個檔名，或先確認舊檔已不需要後自行移走。".into()],
                EXIT_CONFLICT,
            ));
        }
    }
    Ok(())
}

/// 讀取 CA 憑證與私鑰（私鑰必須是 0600）
fn load_ca(cmd: &Cmd) -> Result<(CaKey, PathBuf), Failure> {
    let cert = cmd
        .opt("--ca-cert")
        .ok_or_else(|| usage_err("--ca-cert 必填"))?;
    let key = cmd
        .opt("--ca-key")
        .ok_or_else(|| usage_err("--ca-key 必填"))?;
    if let Some(problem) = ca::ca_key_mode_problem(Path::new(key)) {
        return Err(ca_failure(problem));
    }
    let ca = CaKey::load(&read_file(cert)?, &zeroize::Zeroizing::new(read_file(key)?))
        .map_err(ca_failure)?;
    Ok((ca, PathBuf::from(key)))
}

fn date(ms: i64, env: &Env<'_>) -> String {
    human_time(ms, env.tz)
        .split(' ')
        .next()
        .unwrap_or_default()
        .to_string()
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

pub fn init(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    let now_ms = env.now.timestamp_millis();
    let names = cmd
        .opts_all("--name")
        .iter()
        .map(|n| ServerName::parse(n))
        .collect::<Result<Vec<_>, _>>()
        .map_err(usage_err)?;
    if names.is_empty() {
        return Err(usage_err(
            "至少需要一個 --name（Pulse 連線時使用的 DNS 名稱或 IP；可重複）",
        ));
    }
    let root_dir = cmd.opt("--root-dir").ok_or_else(|| {
        usage_err(
            "--root-dir 必填：根 CA 私鑰寫在這裡，完成後請移到離線保存（例如 /opt/kairos/root-ca）",
        )
    })?;
    let config = load_config(cmd.config_path.as_deref())?;
    let conn = db(cmd)?;
    let plan = InitPlan {
        organization: cmd
            .opt("--organization")
            .unwrap_or(config.pki.organization.trim())
            .to_string(),
        names,
        root_dir: PathBuf::from(root_dir),
        data_dir: PathBuf::from(&config.storage.data_dir),
        server_days: int_opt(cmd, "--server-days")?.unwrap_or(ca::SERVER_DAYS),
    };
    let o = ca::init(&conn, &plan, now_ms).map_err(|e| match e {
        InitError::Conflict(m) => Failure::new(
            "PKI 已經初始化過",
            m,
            vec!["kairos pki init 只用於新安裝；簽發 CA 輪替用 kairos pki issuing-ca new，伺服器憑證到期用 kairos pki sign --profile server。".into()],
            EXIT_CONFLICT,
        ),
        InitError::Usage(m) => usage_err(m),
        InitError::Failed(m) => general("PKI 初始化失敗", m),
    })?;
    let san: Vec<String> = plan.names.iter().map(ServerName::text).collect();
    let _ = store::audit(
        &conn,
        now_ms,
        &store::Audit {
            actor: &operator(),
            category: "pki",
            action: "pki.init",
            object: Some(&o.ca_id.to_string()),
            result: "ok",
            details: Some(json!({
                "root_subject": o.root_subject, "root_pin": o.pin,
                "server_serial": o.server_serial, "server_san": san,
                "issuing_ca_id": o.ca_id, "root_key": o.root_key,
            })),
            ..Default::default()
        },
    );
    if cmd.json {
        let mut out = ok_json(
            cmd,
            env.now,
            json!({
                "root": {"subject": o.root_subject, "cert": o.root_cert, "key": o.root_key,
                         "not_after": crate::api::http::ts_ms(o.root_not_after_ms), "pin": o.pin},
                "server": {"cert": o.server_cert, "key": o.server_key, "serial": o.server_serial,
                           "san": san, "not_after": crate::api::http::ts_ms(o.server_not_after_ms)},
                "issuing_ca": {"ca_id": o.ca_id, "subject": o.issuing_subject, "state": "active",
                               "not_after": crate::api::http::ts_ms(o.issuing_not_after_ms)},
                "config": {"server.cert_path": o.server_cert, "server.key_path": o.server_key,
                           "auth.client_ca": o.client_ca, "pki.mode": "online",
                           "pki.server_ca_path": o.server_ca},
                "warnings": [{"code": CODE_ROOT_KEY_ON_SERVER, "message": "root CA private key is on this server; move it to offline storage"}],
            }),
        );
        out.stderr.clear();
        return Ok(out);
    }
    let m = mark(env);
    let warn = if env.unicode { "!" } else { "[WARN]" };
    Ok(Output {
        stdout: format!(
            "[1/4] {m} 根 CA（ECDSA P-384，10 年，至 {root_end}）：{root_subject}\n\
             \x20     私鑰 {root_key}（0600）\n\
             [2/4] {m} 伺服器憑證（ECDSA P-256，至 {server_end}）：{san}\n\
             \x20     {server_cert}、{server_key}（0600）；EKU serverAuth、CA:FALSE\n\
             [3/4] {m} 代理簽發 CA #{ca_id}（ECDSA P-256，5 年）：已安裝並啟用（線上簽發）\n\
             [4/4] {m} mTLS 信任錨：{client_ca}\n\
             \n\
             CA 指紋（安裝字串的 --ca-pin）：{pin}\n\
             \n\
             在 kairos.toml 設定（相對路徑以 Kairos 的工作目錄解析）：\n\
             \n\
             \x20 [server]\n\
             \x20 cert_path = \"{server_cert}\"\n\
             \x20 key_path = \"{server_key}\"\n\
             \x20 [auth]\n\
             \x20 client_ca = \"{client_ca}\"\n\
             \x20 [pki]\n\
             \x20 mode = \"online\"\n\
             \x20 server_ca_path = \"{server_ca}\"\n\
             \x20 [enroll]\n\
             \x20 mode = \"open\"\n\
             \n\
             {warn} 警告 {code}〔暫定〕：根 CA 私鑰目前在這台伺服器上（{root_dir}）。\n\
             \x20 根 CA 是 Pulse 信任的對象：私鑰外洩時任何人都能冒充 Kairos。請立即把整個目錄複製到離線媒體\n\
             \x20 （加密保存兩份），再從這台主機刪除：shred -u {root_key} && rm -r {root_dir}\n\
             \x20 之後簽發伺服器憑證或新的簽發 CA 時，在離線主機以 kairos pki sign 使用它。\n",
            root_end = date(o.root_not_after_ms, env),
            root_subject = o.root_subject,
            root_key = o.root_key.display(),
            server_end = date(o.server_not_after_ms, env),
            san = san.join("、"),
            server_cert = o.server_cert.display(),
            server_key = o.server_key.display(),
            ca_id = o.ca_id,
            client_ca = o.client_ca.display(),
            pin = o.pin,
            server_ca = o.server_ca.display(),
            code = CODE_ROOT_KEY_ON_SERVER,
            root_dir = plan.root_dir.display(),
        ),
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// sign
// ---------------------------------------------------------------------------

pub fn sign(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    let now_ms = env.now.timestamp_millis();
    let profile = cmd.opt("--profile").ok_or_else(|| {
        usage_err("--profile 必填：server（伺服器憑證）、issuing-ca（代理簽發 CA）或 client（用戶端憑證）")
    })?;
    let out = cmd.opt("--out").ok_or_else(|| usage_err("--out 必填"))?;
    let new_key = cmd.opt("--new-key");
    let csr_path = cmd.opt("--csr");
    match (profile, csr_path, new_key) {
        ("server", Some(_), Some(_)) => return Err(usage_err("--csr 與 --new-key 只能擇一")),
        ("server", None, None) => {
            return Err(usage_err(
                "伺服器憑證需要 --csr <檔案>（Kairos 主機產生的 CSR）或 --new-key <檔案>（在這裡產生新的私鑰）",
            ));
        }
        ("issuing-ca" | "client", None, _) => return Err(usage_err("--csr 必填")),
        ("issuing-ca" | "client", _, Some(_)) => {
            return Err(usage_err("--new-key 只用於 --profile server"));
        }
        ("server" | "issuing-ca" | "client", _, _) => {}
        (p, _, _) => {
            return Err(usage_err(format!(
                "--profile「{p}」無效：server、issuing-ca 或 client"
            )));
        }
    }
    refuse_existing(
        &[Some(out), new_key]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>(),
    )?;
    let (ca, key_path) = load_ca(cmd)?;
    let csr = csr_path
        .map(|p| ca::parse_csr(&read_file(p)?).map_err(|e| usage_err(format!("{p}：{e}"))))
        .transpose()?;
    let mut generated_key = None;
    let (issued, subject_note) = match profile {
        "server" => {
            let names = cmd
                .opts_all("--name")
                .iter()
                .map(|n| ServerName::parse(n))
                .collect::<Result<Vec<_>, _>>()
                .map_err(usage_err)?;
            if names.is_empty() {
                return Err(usage_err("伺服器憑證至少需要一個 --name（可重複）"));
            }
            let spki = match &csr {
                Some(c) => c.spki_der.clone(),
                None => {
                    let (pem, spki) = ca::new_p256_key().map_err(|e| general("無法產生私鑰", e))?;
                    generated_key = Some(pem);
                    spki
                }
            };
            let san: Vec<String> = names.iter().map(ServerName::text).collect();
            (
                ca.sign_server(&spki, &names, days_opt(cmd, ca::SERVER_DAYS)?, now_ms)
                    .map_err(usage_err)?,
                json!({"profile": "server", "san": san}),
            )
        }
        "issuing-ca" => {
            let c = csr.as_ref().ok_or_else(|| usage_err("--csr 必填"))?;
            let cn = c.common_name.clone().unwrap_or_default();
            (
                ca.sign_issuing_ca(&c.spki_der, &cn, days_opt(cmd, ca::ISSUING_DAYS)?, now_ms)
                    .map_err(usage_err)?,
                json!({"profile": "issuing-ca", "subject": format!("CN={cn}")}),
            )
        }
        _ => {
            let c = csr.as_ref().ok_or_else(|| usage_err("--csr 必填"))?;
            let id = cmd
                .opt("--agent-id")
                .ok_or_else(|| usage_err("--agent-id 必填（對方提供的 agent_id，小寫 UUID）"))?
                .to_ascii_lowercase();
            if c.common_name.as_deref().is_some_and(|cn| cn != id) {
                return Err(usage_err(format!(
                    "CSR 的 CN「{}」與 --agent-id 不同；向對方確認後再簽",
                    c.common_name.as_deref().unwrap_or_default()
                )));
            }
            (
                ca.sign_client(&c.spki_der, &id, days_opt(cmd, ca::CLIENT_DAYS)?, now_ms)
                    .map_err(usage_err)?,
                json!({"profile": "client", "agent_id": id}),
            )
        }
    };
    // 先寫紀錄，寫不進去就不交出憑證（與 openssl ca 的 index.txt 相同的作用）
    let journal = ca::journal_path(&key_path);
    let mut entry = subject_note.clone();
    if let Some(m) = entry.as_object_mut() {
        m.insert("serial".into(), json!(issued.serial));
        m.insert("issuer".into(), json!(ca.info.subject_text));
        m.insert("issuer_spki_sha256".into(), json!(ca.spki_sha256_hex()));
        m.insert(
            "not_after".into(),
            json!(kairos_proto::enroll::format_rfc3339_ms(issued.not_after_ms)),
        );
        m.insert("out".into(), json!(out));
    }
    ca::journal_append(
        &journal,
        &ca::journal_entry("sign", &operator(), now_ms, entry),
    )
    .map_err(|e| general("無法寫入簽發紀錄，未簽發", e))?;
    if let (Some(path), Some(pem)) = (new_key, &generated_key) {
        crate::pki::write_private_key(Path::new(path), pem)
            .map_err(|e| general(&format!("無法寫入 {path}"), e.to_string()))?;
    }
    write_new_file(out, pem_encode("CERTIFICATE", &issued.der).as_bytes())?;
    if cmd.json {
        return Ok(ok_json(
            cmd,
            env.now,
            json!({"serial": issued.serial, "out": out, "new_key": new_key,
                   "not_after": crate::api::http::ts_ms(issued.not_after_ms),
                   "issuer": ca.info.subject_text, "details": subject_note,
                   "journal": journal}),
        ));
    }
    let m = mark(env);
    let what = match profile {
        "server" => format!(
            "伺服器憑證（{}；EKU serverAuth、CA:FALSE）",
            subject_note["san"]
                .as_array()
                .map(|a| a
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join("、"))
                .unwrap_or_default()
        ),
        "issuing-ca" => format!(
            "代理簽發 CA（{}；CA:TRUE、pathlen:0、EKU clientAuth）",
            subject_note["subject"].as_str().unwrap_or_default()
        ),
        _ => format!(
            "用戶端憑證（SAN DNS:{}；EKU clientAuth）",
            subject_note["agent_id"].as_str().unwrap_or_default()
        ),
    };
    let next = match profile {
        "server" => "  下一步：放到 Kairos 的 server.cert_path（與 server.key_path），Kairos 在下一次重載週期自動載入\n".to_string(),
        "issuing-ca" => format!("  下一步：sudo -u kairos /opt/kairos/kairos pki issuing-ca install {out}\n"),
        _ => "  下一步：交給該台 Pulse（client.pem）；這張憑證不在 Kairos 的簽發紀錄中，撤銷用 kairos pki gencrl\n".to_string(),
    };
    Ok(Output {
        stdout: format!(
            "{m} 已簽發{what}\n  序號 {}，有效至 {}\n  簽發者 {}（{}）\n  寫入 {out}{}\n  紀錄 {}\n{next}",
            issued.serial,
            date(issued.not_after_ms, env),
            ca.info.subject_text,
            ca.key_alg,
            new_key
                .map(|k| format!("；新的私鑰 {k}（0600）"))
                .unwrap_or_default(),
            journal.display(),
        ),
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// gencrl
// ---------------------------------------------------------------------------

pub fn gencrl(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    if cmd.opt("--ca-key").is_none() && cmd.opt("--ca-cert").is_none() {
        return gencrl_online(cmd, env);
    }
    let now_ms = env.now.timestamp_millis();
    let out = cmd.opt("--out").ok_or_else(|| usage_err("--out 必填"))?;
    refuse_existing(&[out])?;
    let days = days_opt(cmd, ca::CRL_DAYS)?;
    if !(1..=365).contains(&days) {
        return Err(usage_err("--days 必須是 1～365"));
    }
    let (ca, key_path) = load_ca(cmd)?;
    let revoked = match cmd.opt("--revocations") {
        Some(p) => {
            let v: serde_json::Value = serde_json::from_str(&read_file(p)?)
                .map_err(|e| usage_err(format!("{p} 不是 JSON：{e}")))?;
            ca::parse_revocations(&v, &ca).map_err(usage_err)?
        }
        None => Vec::new(),
    };
    let journal = ca::journal_path(&key_path);
    let spki = ca.spki_sha256_hex();
    let number = ca::next_offline_crl_number(&journal, &spki, now_ms);
    let next_update = (now_ms + days * ca::DAY_MS).min(ca.info.not_after_ms);
    if next_update <= now_ms {
        return Err(ca_failure("CA 憑證已過期，無法產生 CRL"));
    }
    let der = ca
        .sign_crl(number, now_ms, next_update, &revoked)
        .map_err(ca_failure)?;
    ca::journal_append(
        &journal,
        &ca::journal_entry(
            "gencrl",
            &operator(),
            now_ms,
            json!({"issuer": ca.info.subject_text, "issuer_spki_sha256": spki,
                   "crl_number": number, "revoked": revoked.len(), "out": out,
                   "next_update": kairos_proto::enroll::format_rfc3339_ms(next_update)}),
        ),
    )
    .map_err(|e| general("無法寫入簽發紀錄，未產生 CRL", e))?;
    write_new_file(out, pem_encode("X509 CRL", &der).as_bytes())?;
    if cmd.json {
        return Ok(ok_json(
            cmd,
            env.now,
            json!({"crl_number": number, "revoked": revoked.len(), "out": out,
                   "next_update": crate::api::http::ts_ms(next_update), "issuer": ca.info.subject_text}),
        ));
    }
    Ok(Output {
        stdout: format!(
            "{} 已產生 CRL 第 {number} 版（{} 筆撤銷），下次更新 {}\n  簽發者 {}\n  寫入 {out}；紀錄 {}\n  下一步：部署到 Kairos 的 auth.crl（Kairos 在下一次重載週期載入）；nextUpdate 之前要再產生一次\n",
            mark(env),
            revoked.len(),
            human_time(next_update, env.tz),
            ca.info.subject_text,
            journal.display(),
        ),
        ..Default::default()
    })
}

/// 線上模式：與主控台 S15「立即產生 CRL」相同（`PkiManager::regenerate_crl`、`write_crl_file`）
fn gencrl_online(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    let now_ms = env.now.timestamp_millis();
    let config = load_config(cmd.config_path.as_deref())?;
    if config.pki.effective_mode() != crate::config::PkiMode::Online {
        return Err(usage_err(
            "離線簽發模式（pki.mode = \"offline\"）：Kairos 沒有 CA 私鑰。在 CA 主機以 kairos pki gencrl --ca-cert … --ca-key … --revocations … 產生",
        ));
    }
    let mut conn = db(cmd)?;
    let data_dir = PathBuf::from(&config.storage.data_dir);
    let mgr = crate::enroll::pki_mgr::PkiManager::new(&data_dir, config.pki.effective_mode());
    mgr.sync_cas(&conn)
        .map_err(|e| db_failure(format!("{e:?}")))?;
    let ids = mgr.issuer_ids();
    if ids.is_empty() {
        return Err(Failure {
            code: Some("KRS-E1303"),
            ..Failure::new(
                "線上簽發不可用，無法產生 CRL",
                match mgr.status() {
                    crate::enroll::pki_mgr::IssueStatus::Unavailable(r) => r,
                    _ => "沒有可用的簽發 CA 私鑰".to_string(),
                },
                vec!["以 Kairos 的服務帳號執行（sudo -u kairos），並確認 data/pki/ 為 0700、私鑰為 0600。".into()],
                EXIT_GENERAL,
            )
        });
    }
    let validity = i64::from(config.pki.crl_validity_days) * ca::DAY_MS;
    let actor = operator();
    let mut made = Vec::new();
    for id in ids {
        match mgr.regenerate_crl(&mut conn, id, validity, "manual", Some(&actor), now_ms) {
            Ok(Some(n)) => made.push((id, n)),
            Ok(None) => {}
            Err(e) => return Err(general("CRL 產生失敗", format!("簽發 CA {id}：{e:?}"))),
        }
    }
    mgr.write_crl_file(&conn)
        .map_err(|e| db_failure(format!("{e:?}")))?;
    let _ = store::audit(
        &conn,
        now_ms,
        &store::Audit {
            actor: &actor,
            category: "pki",
            action: "crl.generate",
            result: "ok",
            details: Some(
                json!({"crls": made.iter().map(|(id, n)| json!({"ca_id": id, "crl_number": n})).collect::<Vec<_>>(), "via": "cli"}),
            ),
            ..Default::default()
        },
    );
    if cmd.json {
        return Ok(ok_json(
            cmd,
            env.now,
            json!({"crls": made.iter().map(|(id, n)| json!({"ca_id": id, "crl_number": n})).collect::<Vec<_>>(),
                   "file": crate::pki::issuing_crl_path(&data_dir)}),
        ));
    }
    let mut text = String::new();
    for (id, n) in &made {
        text.push_str(&format!("{} 簽發 CA #{id}：CRL 第 {n} 版\n", mark(env)));
    }
    text.push_str(&format!(
        "  寫入 {}；執行中的 Kairos 在下一次重載週期（server.reload_interval_secs）載入\n",
        crate::pki::issuing_crl_path(&data_dir).display()
    ));
    Ok(Output {
        stdout: text,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// revocations export
// ---------------------------------------------------------------------------

pub fn revocations_export(cmd: &Cmd, env: &mut Env<'_>) -> Result<Output, Failure> {
    let now_ms = env.now.timestamp_millis();
    let ca_cert = cmd.opt("--ca-cert").ok_or_else(|| {
        usage_err("--ca-cert 必填：要產生 CRL 的 CA（離線模式通常是 auth.client_ca 中的代理 CA）")
    })?;
    let der = pem_decode_all(&read_file(ca_cert)?, Some("CERTIFICATE"))
        .map_err(usage_err)?
        .into_iter()
        .next()
        .ok_or_else(|| usage_err(format!("{ca_cert} 中找不到憑證")))?
        .1;
    let info = parse_cert(&der).map_err(usage_err)?;
    let conn = db(cmd)?;
    let v = ca::export_revocations(&conn, &info, now_ms).map_err(db_failure)?;
    let n = v["revoked"].as_array().map_or(0, Vec::len);
    let text = serde_json::to_string_pretty(&v).unwrap_or_default() + "\n";
    match cmd.opt("--out") {
        Some(out) => {
            write_new_file(out, text.as_bytes())?;
            Ok(Output {
                stdout: if cmd.json {
                    ok_json(cmd, env.now, json!({"revoked": n, "out": out})).stdout
                } else {
                    String::new()
                },
                stderr: if cmd.json {
                    String::new()
                } else {
                    format!(
                        "{} 已匯出 {n} 筆撤銷（{}）到 {out}；在 CA 主機執行 kairos pki gencrl --revocations {out} …\n",
                        mark(env),
                        info.subject_text
                    )
                },
                exit: 0,
            })
        }
        None => Ok(Output {
            stdout: text,
            stderr: format!("已匯出 {n} 筆撤銷（{}）\n", info.subject_text),
            exit: 0,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enroll_cli::{EXIT_USAGE, Group, execute, parse_args};
    use chrono::{FixedOffset, TimeZone, Utc};
    use std::ffi::OsString;

    fn run(args: &[&str]) -> Output {
        let cmd = parse_args(Group::Pki, args.iter().map(OsString::from)).unwrap();
        let mut confirm = |_: &str| -> Option<String> { None };
        let mut env = Env {
            now: Utc.timestamp_millis_opt(1_760_000_000_000).unwrap(),
            tz: FixedOffset::east_opt(8 * 3600).unwrap(),
            unicode: false,
            confirm: &mut confirm,
        };
        execute(&cmd, &mut env)
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kairos-pki-cli-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 寫出測試用的根 CA（私鑰 0600）
    fn write_root(dir: &Path) -> (String, String) {
        let root = ca::new_root("Test", 1_760_000_000_000).unwrap();
        let cert = dir.join("root.pem");
        let key = dir.join("root.key");
        std::fs::write(&cert, pem_encode("CERTIFICATE", &root.cert_der)).unwrap();
        // CaKey 不公開私鑰：以 PEM 往返取得
        crate::pki::write_private_key(&key, &root_key_pem(&root)).unwrap();
        (
            cert.to_string_lossy().into_owned(),
            key.to_string_lossy().into_owned(),
        )
    }

    fn root_key_pem(root: &CaKey) -> String {
        root.key_pem().to_string()
    }

    #[test]
    fn sign_server_with_new_key_writes_journal_and_never_overwrites() {
        let d = tmp("sign");
        let (cert, key) = write_root(&d);
        let out = d.join("server.pem").to_string_lossy().into_owned();
        let nk = d.join("server.key").to_string_lossy().into_owned();
        let args = [
            "sign",
            "--profile",
            "server",
            "--ca-cert",
            &cert,
            "--ca-key",
            &key,
            "--name",
            "monitor.example.com",
            "--name",
            "10.0.0.5",
            "--new-key",
            &nk,
            "--out",
            &out,
        ];
        let o = run(&args);
        assert_eq!(o.exit, 0, "{}{}", o.stdout, o.stderr);
        assert!(o.stdout.contains("DNS:monitor.example.com"), "{}", o.stdout);
        let pem = std::fs::read_to_string(&out).unwrap();
        let der = pem_decode_all(&pem, None).unwrap().remove(0).1;
        let i = parse_cert(&der).unwrap();
        assert_eq!(i.eku, Some((false, true, false)));
        assert!(!i.is_ca);
        // 新私鑰與憑證成對、0600
        let k = rcgen::KeyPair::from_pem(&std::fs::read_to_string(&nk).unwrap()).unwrap();
        assert_eq!(
            rcgen::PublicKeyData::subject_public_key_info(&k),
            i.spki_der
        );
        assert_eq!(ca::ca_key_mode_problem(Path::new(&nk)), None);
        // 紀錄
        let log = std::fs::read_to_string(d.join(ca::JOURNAL_FILE)).unwrap();
        assert!(
            log.contains("\"action\":\"sign\"") && log.contains(&i.serial),
            "{log}"
        );
        // 再執行一次：輸出檔已存在，不覆寫
        let again = run(&args);
        assert_eq!(again.exit, EXIT_CONFLICT, "{}", again.stderr);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), pem);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn loose_ca_key_permission_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("perm");
        let (cert, key) = write_root(&d);
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let out = d.join("x.crl").to_string_lossy().into_owned();
        let o = run(&[
            "gencrl",
            "--ca-cert",
            &cert,
            "--ca-key",
            &key,
            "--out",
            &out,
        ]);
        assert_eq!(o.exit, EXIT_CERT);
        assert!(o.stderr.contains(CODE_CA_UNUSABLE), "{}", o.stderr);
        assert!(!Path::new(&out).exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn offline_gencrl_with_and_without_revocations() {
        let d = tmp("gencrl");
        let (cert, key) = write_root(&d);
        let first = d.join("a.crl").to_string_lossy().into_owned();
        let o = run(&[
            "gencrl",
            "--ca-cert",
            &cert,
            "--ca-key",
            &key,
            "--out",
            &first,
        ]);
        assert_eq!(o.exit, 0, "{}", o.stderr);
        let root = CaKey::load(
            &std::fs::read_to_string(&cert).unwrap(),
            &std::fs::read_to_string(&key).unwrap(),
        )
        .unwrap();
        let rev = d.join("rev.json");
        std::fs::write(
            &rev,
            json!({"schema": ca::REVOCATIONS_SCHEMA, "issuer": "x", "issuer_spki_sha256": root.spki_sha256_hex(),
                   "revoked": [{"serial": "5E0B2C77A1D94F3E8B6A0C21D4F7E913", "revoked_at": "2025-10-01T00:00:00Z", "reason": "superseded"}]})
                .to_string(),
        )
        .unwrap();
        let second = d.join("b.crl").to_string_lossy().into_owned();
        let o = run(&[
            "gencrl",
            "--ca-cert",
            &cert,
            "--ca-key",
            &key,
            "--revocations",
            &rev.to_string_lossy(),
            "--out",
            &second,
            "--json",
        ]);
        assert_eq!(o.exit, 0, "{}", o.stdout);
        let v: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
        assert_eq!(v["schema"], "kairos.pki.gencrl/v1");
        assert_eq!(v["data"]["revoked"], 1);
        // 同一秒內第二版：版本仍遞增
        let numbers: Vec<u64> = [&first, &second]
            .iter()
            .map(|p| {
                let der = pem_decode_all(&std::fs::read_to_string(p).unwrap(), None)
                    .unwrap()
                    .remove(0)
                    .1;
                let (_, crl) = x509_parser::parse_x509_crl(&der).unwrap();
                crl.crl_number().unwrap().to_u64_digits()[0]
            })
            .collect();
        assert!(numbers[1] > numbers[0], "{numbers:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn usage_errors_are_reported() {
        assert_eq!(run(&["sign", "--out", "x"]).exit, EXIT_USAGE);
        assert_eq!(
            run(&["sign", "--profile", "bogus", "--out", "x"]).exit,
            EXIT_USAGE
        );
        assert_eq!(
            run(&["sign", "--profile", "server", "--out", "x"]).exit,
            EXIT_USAGE
        );
        assert_eq!(run(&["init", "--root-dir", "x"]).exit, EXIT_USAGE);
        assert_eq!(
            run(&["init", "--name", "*.bad", "--root-dir", "x"]).exit,
            EXIT_USAGE
        );
        let c = parse_args(
            Group::Pki,
            ["revocations", "export"].iter().map(OsString::from),
        )
        .unwrap();
        assert_eq!(c.schema(), "kairos.pki.revocations.export/v1");
    }
}
