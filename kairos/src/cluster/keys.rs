//! 叢集的金鑰一致性（`kairos/HA.md` 第 11 節）。
//!
//! 所有節點必須持有同一組金鑰：四把對稱金鑰（`alerts-secrets.key`、`console.key`、`audit-hmac.key`、
//! `privacy.key`）與狀態為 `active`／`trusted` 的簽發 CA 私鑰（`pki/issuing-<ca_id>.key`）。
//!
//! - **指紋**：對稱金鑰是 `HMAC-SHA256(金鑰, "kairos-key-fingerprint/v1\n" ‖ 檔名)` 的前 16 bytes（hex）；
//!   簽發 CA 私鑰是公鑰的 SPKI SHA-256（hex），與 `pki_cas.spki_sha256` 比對。指紋不能用來還原金鑰。
//! - `kairos cluster init` 把對稱金鑰的指紋寫入 `meta.cluster_keys`；每個節點啟動時（以及之後定期）
//!   計算自己的指紋並比對，**缺少或不符時不參與 leader 選舉**（`KRS-E8504`，`/readyz` 的 `keys: mismatch`）。
//! - **叢集模式下停用所有金鑰的自動建立**（[`disable_autocreate`]）：`SecretBox::open`、
//!   `ConsoleKey::load_or_create`、`AuditKey::load_or_create`、`PseudonymKey::load_or_create` 找不到金鑰檔時
//!   回錯誤而不是建立新的。只有 `kairos cluster init` 會建立（[`create_missing`]）。

use rcgen::PublicKeyData;
use ring::rand::SecureRandom;
use std::collections::BTreeMap;
use std::io::{Error, ErrorKind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use super::CODE_KEYS;
use crate::pki::store::CaRow;

/// 需要一致的對稱金鑰檔（資料目錄下；與金鑰備份相同的四個）
pub const SYMMETRIC_KEY_FILES: [&str; 4] = crate::keys_backup::KEY_FILES;

const FINGERPRINT_LABEL: &[u8] = b"kairos-key-fingerprint/v1\n";

static NO_AUTOCREATE: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static NO_AUTOCREATE_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 叢集模式：之後這個行程不再自動建立金鑰檔
#[cfg_attr(test, allow(dead_code))]
pub fn disable_autocreate() {
    NO_AUTOCREATE.store(true, Ordering::SeqCst);
}

/// 測試用：只影響目前的執行緒
#[cfg(test)]
pub fn set_autocreate_disabled_for_test(disabled: bool) {
    NO_AUTOCREATE_TEST.with(|c| c.set(disabled));
}

fn autocreate_disabled() -> bool {
    #[cfg(test)]
    if NO_AUTOCREATE_TEST.with(std::cell::Cell::get) {
        return true;
    }
    NO_AUTOCREATE.load(Ordering::SeqCst)
}

/// 金鑰檔 `path` 不存在、正要自動建立時呼叫：叢集模式下回錯誤（不建立），否則 `Ok`
pub fn check_autocreate(path: &Path) -> Result<(), Error> {
    if autocreate_disabled() {
        return Err(Error::new(
            ErrorKind::NotFound,
            format!(
                "{CODE_KEYS}：找不到金鑰檔 {}。叢集模式（cluster.enabled = true）下不自動建立金鑰：每個節點必須持有同一組金鑰；請以 kairos cluster keys import 匯入叢集的金鑰（第一個節點以 kairos cluster init 建立）",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// 對稱金鑰的指紋
pub fn symmetric_fingerprint(key: &[u8], name: &str) -> String {
    let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    let mut ctx = ring::hmac::Context::with_key(&k);
    ctx.update(FINGERPRINT_LABEL);
    ctx.update(name.as_bytes());
    hex(&ctx.sign().as_ref()[..16])
}

/// 簽發 CA 私鑰（PKCS#8 PEM）的指紋：公鑰的 SPKI SHA-256
pub fn ca_key_fingerprint(key_pem: &str) -> Result<String, String> {
    let key = rcgen::KeyPair::from_pem(key_pem).map_err(|e| e.to_string())?;
    Ok(hex(ring::digest::digest(
        &ring::digest::SHA256,
        &key.subject_public_key_info(),
    )
    .as_ref()))
}

/// 這個節點的金鑰指紋
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalKeys {
    /// 對稱金鑰：檔名 → 指紋（不存在的不列）
    pub symmetric: BTreeMap<String, String>,
    /// 簽發 CA 私鑰：相對於資料目錄的檔名（`pki/issuing-<id>.key`）→ SPKI SHA-256
    pub ca: BTreeMap<String, String>,
    /// 無法讀取或格式不對的金鑰檔
    pub unreadable: Vec<String>,
}

impl LocalKeys {
    /// 讀取資料目錄中的金鑰檔並計算指紋
    pub fn read(data_dir: &Path) -> Self {
        let mut out = Self::default();
        for name in SYMMETRIC_KEY_FILES {
            let path = data_dir.join(name);
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let bytes = zeroize::Zeroizing::new(bytes);
                    if bytes.len() == 32 {
                        out.symmetric
                            .insert(name.to_string(), symmetric_fingerprint(&bytes, name));
                    } else {
                        out.unreadable
                            .push(format!("{name}（長度不是 32 bytes，檔案可能已損毀）"));
                    }
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => out.unreadable.push(format!("{name}（{e}）")),
            }
        }
        let pki = crate::pki::pki_dir(data_dir);
        if let Ok(entries) = std::fs::read_dir(&pki) {
            for entry in entries.flatten() {
                let file = entry.file_name().to_string_lossy().into_owned();
                let is_key = file
                    .strip_prefix("issuing-")
                    .and_then(|r| r.strip_suffix(".key"))
                    .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()));
                if !is_key {
                    continue;
                }
                let rel = format!("{}/{file}", crate::pki::PKI_DIR);
                match std::fs::read_to_string(entry.path()) {
                    Ok(pem) => {
                        let pem = zeroize::Zeroizing::new(pem);
                        match ca_key_fingerprint(&pem) {
                            Ok(fp) => {
                                out.ca.insert(rel, fp);
                            }
                            Err(e) => out.unreadable.push(format!("{rel}（{e}）")),
                        }
                    }
                    Err(e) => out.unreadable.push(format!("{rel}（{e}）")),
                }
            }
        }
        out
    }

    /// 指紋的雜湊（`cluster_nodes.keys_hash`：兩個節點的值相同＝金鑰相同）
    pub fn hash(&self) -> String {
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
        for (name, fp) in self.symmetric.iter().chain(self.ca.iter()) {
            ctx.update(name.as_bytes());
            ctx.update(b"=");
            ctx.update(fp.as_bytes());
            ctx.update(b"\n");
        }
        hex(&ctx.finish().as_ref()[..16])
    }

    /// `meta.cluster_keys` 的內容（`kairos cluster init` 寫入）：對稱金鑰的指紋
    pub fn to_meta(&self) -> String {
        serde_json::json!({ "v": 1, "keys": self.symmetric }).to_string()
    }

    /// 與叢集記錄的指紋比對；回傳問題清單（空的＝一致）。
    ///
    /// - `meta`：`meta.cluster_keys`（`None`＝尚未執行 `kairos cluster init`）
    /// - `cas`：`pki_cas` 的所有列；狀態為 `active`／`trusted` 且有 `key_file` 的簽發 CA 必須有私鑰
    pub fn compare(&self, meta: Option<&str>, cas: &[CaRow]) -> Vec<String> {
        let mut problems: Vec<String> = self
            .unreadable
            .iter()
            .map(|u| format!("金鑰檔無法使用：{u}"))
            .collect();
        let Some(meta) = meta else {
            problems.push("資料庫中沒有叢集的金鑰指紋（尚未執行 kairos cluster init）".to_string());
            return problems;
        };
        let expected: BTreeMap<String, String> = serde_json::from_str::<serde_json::Value>(meta)
            .ok()
            .and_then(|v| serde_json::from_value(v.get("keys")?.clone()).ok())
            .unwrap_or_default();
        if expected.is_empty() {
            problems.push("資料庫中的叢集金鑰指紋（meta.cluster_keys）無法解析".to_string());
            return problems;
        }
        for (name, fp) in &expected {
            match self.symmetric.get(name) {
                None => problems.push(format!("缺少 {name}")),
                Some(local) if local != fp => {
                    problems.push(format!("{name} 與叢集的金鑰不同"));
                }
                Some(_) => {}
            }
        }
        for ca in cas {
            if !matches!(ca.state.as_str(), "active" | "trusted") {
                continue;
            }
            let Some(file) = ca.key_file.as_deref().filter(|f| !f.is_empty()) else {
                continue;
            };
            let want = hex(&ca.spki_sha256);
            match self.ca.get(file) {
                None => problems.push(format!("缺少簽發 CA #{} 的私鑰 {file}", ca.ca_id)),
                Some(local) if *local != want => problems.push(format!(
                    "簽發 CA #{} 的私鑰 {file} 與資料庫中的憑證不成對",
                    ca.ca_id
                )),
                Some(_) => {}
            }
        }
        problems
    }
}

/// `kairos cluster init`：建立還不存在的對稱金鑰檔（32 bytes 隨機值，0600）；回傳新建立的檔名
pub fn create_missing(data_dir: &Path) -> Result<Vec<&'static str>, Error> {
    let rng = ring::rand::SystemRandom::new();
    let mut created = Vec::new();
    for name in SYMMETRIC_KEY_FILES {
        let path = data_dir.join(name);
        if path.exists() {
            continue;
        }
        let mut key = zeroize::Zeroizing::new([0u8; 32]);
        rng.fill(&mut key[..])
            .map_err(|_| Error::other("無法取得隨機數（作業系統的 CSPRNG）"))?;
        crate::keys_backup::write_new_private(&path, &key[..])?;
        created.push(name);
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "kairos-clkeys-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn fingerprints_depend_on_key_and_name_and_do_not_reveal_the_key() {
        let a = symmetric_fingerprint(&[1u8; 32], "console.key");
        assert_eq!(a.len(), 32);
        assert_eq!(a, symmetric_fingerprint(&[1u8; 32], "console.key"));
        assert_ne!(a, symmetric_fingerprint(&[2u8; 32], "console.key"));
        assert_ne!(a, symmetric_fingerprint(&[1u8; 32], "audit-hmac.key"));
        assert!(!a.contains("0101010101"));
    }

    #[test]
    fn init_creates_keys_and_a_copy_matches_while_a_fresh_set_does_not() {
        let a = temp("a");
        let created = create_missing(&a).unwrap();
        assert_eq!(created.len(), 4);
        // 再執行一次不會改動既有的金鑰
        assert!(create_missing(&a).unwrap().is_empty());
        let local_a = LocalKeys::read(&a);
        assert_eq!(local_a.symmetric.len(), 4);
        let meta = local_a.to_meta();
        assert!(local_a.compare(Some(&meta), &[]).is_empty());

        // 節點 B：複製同一組金鑰 → 一致、keys_hash 相同
        let b = temp("b");
        for name in SYMMETRIC_KEY_FILES {
            std::fs::copy(a.join(name), b.join(name)).unwrap();
        }
        let local_b = LocalKeys::read(&b);
        assert!(local_b.compare(Some(&meta), &[]).is_empty());
        assert_eq!(local_a.hash(), local_b.hash());

        // 節點 C：自己產生的金鑰 → 每一把都不同；少一把時回報缺少
        let c = temp("c");
        create_missing(&c).unwrap();
        std::fs::remove_file(c.join("privacy.key")).unwrap();
        let local_c = LocalKeys::read(&c);
        let problems = local_c.compare(Some(&meta), &[]);
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems.iter().any(|p| p == "缺少 privacy.key"));
        assert!(
            problems
                .iter()
                .any(|p| p.contains("console.key 與叢集的金鑰不同"))
        );
        assert_ne!(local_a.hash(), local_c.hash());

        // 還沒有 cluster init
        assert!(
            local_a.compare(None, &[])[0].contains("kairos cluster init"),
            "{:?}",
            local_a.compare(None, &[])
        );
        for d in [a, b, c] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn issuing_ca_keys_are_matched_against_the_database() {
        let dir = temp("ca");
        std::fs::create_dir_all(dir.join("pki")).unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        std::fs::write(dir.join("pki/issuing-1.key"), key.serialize_pem()).unwrap();
        // 不是金鑰檔名的檔案不看
        std::fs::write(dir.join("pki/issuing-cas.pem"), "x").unwrap();
        create_missing(&dir).unwrap();
        let local = LocalKeys::read(&dir);
        assert_eq!(local.ca.len(), 1);
        let meta = local.to_meta();
        let spki = ring::digest::digest(&ring::digest::SHA256, &key.subject_public_key_info())
            .as_ref()
            .to_vec();
        let ca = |id: i64, state: &str, spki: Vec<u8>| CaRow {
            ca_id: id,
            subject: "CN=x".into(),
            spki_sha256: spki,
            cert_der: None,
            not_before: None,
            not_after: None,
            key_file: Some(crate::pki::key_file_name(id)),
            state: state.into(),
            next_crl_number: 1,
            created_at: 0,
            retired_at: None,
        };
        assert!(
            local
                .compare(Some(&meta), &[ca(1, "active", spki.clone())])
                .is_empty()
        );
        // 另一把啟用中的簽發 CA 沒有私鑰
        let p = local.compare(
            Some(&meta),
            &[ca(1, "active", spki.clone()), ca(2, "trusted", vec![9; 32])],
        );
        assert_eq!(
            p,
            vec!["缺少簽發 CA #2 的私鑰 pki/issuing-2.key".to_string()]
        );
        // 已退役或等待簽發的不要求
        assert!(
            local
                .compare(
                    Some(&meta),
                    &[
                        ca(1, "active", spki),
                        ca(2, "retired", vec![9; 32]),
                        ca(3, "pending", vec![8; 32])
                    ]
                )
                .is_empty()
        );
        // 私鑰與資料庫中的憑證不成對
        let p = local.compare(Some(&meta), &[ca(1, "active", vec![7; 32])]);
        assert!(p[0].contains("不成對"), "{p:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn autocreate_is_refused_in_cluster_mode() {
        let path = Path::new("/nonexistent/console.key");
        assert!(check_autocreate(path).is_ok());
        set_autocreate_disabled_for_test(true);
        let e = check_autocreate(path).unwrap_err();
        assert!(e.to_string().contains(CODE_KEYS), "{e}");
        assert_eq!(e.kind(), ErrorKind::NotFound);
        // 四個建立金鑰的函式都不建立
        let dir = temp("noauto");
        assert!(crate::alerts::secrets::SecretBox::open(&dir).is_err());
        assert!(crate::users::secrets::ConsoleKey::load_or_create(&dir).is_err());
        assert!(crate::audit::AuditKey::load_or_create(&dir).is_err());
        assert!(crate::privacy::PseudonymKey::load_or_create(&dir).is_err());
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
        set_autocreate_disabled_for_test(false);
        assert!(crate::users::secrets::ConsoleKey::load_or_create(&dir).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }
}
