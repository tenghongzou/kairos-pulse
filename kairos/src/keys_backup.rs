//! 資料目錄金鑰檔的加密備份（`kairos db backup --include-keys`、`kairos db restore-keys`；
//! 設計包 `decisions/INTEGRATION.md` §28.5 #8、§28.9 #3）。
//!
//! 備份的金鑰檔（存在的才備份）：`alerts-secrets.key`（通知管道的機密）、`console.key`（TOTP 秘密、
//! CSRF、復原碼）、`audit-hmac.key`（稽核鏈的 HMAC）。預設**不**備份；需要另外指定，並以使用者提供的
//! 備份密碼加密，寫成與資料庫備份檔並列的 `<備份檔>.keys`（0600）。這樣資料庫備份檔本身仍不含任何
//! 金鑰，可以分開保存（例如金鑰備份放在另一個保管位置）。
//!
//! ## 格式（`kairos.keys-backup/v1`，一份 JSON）
//!
//! ```text
//! {"format":"kairos.keys-backup/v1","kdf":{"alg":"argon2id","m_kib":65536,"t":3,"p":1,"salt":"<b64url>"},
//!  "cipher":"aes-256-gcm","nonce":"<b64url>","ciphertext":"<b64url>","created_at":"…","kairos_version":"…"}
//! ```
//!
//! - 金鑰：Argon2id（m=64 MiB、t=3、p=1、16 bytes 隨機鹽）由備份密碼衍生 32 bytes；加密：AES-256-GCM
//!   （12 bytes 隨機 nonce）。都是既有的依賴（`argon2`、`ring`），不引入 age 等新的 crate。
//! - 附加資料（AAD）是 `format`、`kdf`、`cipher` 的正規 JSON：竄改參數、鹽或 nonce 都會使解密失敗。
//! - 明文是簡單的二進位串列（名稱長度、名稱、內容長度、內容），不經過 JSON 字串，整段在記憶體中用完
//!   即清除（`zeroize`）；密碼也一樣。錯誤的密碼與被修改的檔案無法區分（同一個錯誤，
//!   [`CODE_KEYS_BACKUP`]）。
//! - 讀取時限制 KDF 參數的上限（避免被修改的檔案要求數十 GB 記憶體）。

use argon2::{Algorithm, Argon2, Params, Version};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};
use std::io::{Error, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

use crate::api::http::{b64url_decode, b64url_encode};

/// 金鑰檔的加密備份無法建立、解開或還原（密碼錯誤、檔案被修改、格式不符、無法寫回資料目錄）；
/// 子類別 43「匯出、匯入與備份」（設計包 `decisions/INTEGRATION.md` §29.3 正式分配）
pub const CODE_KEYS_BACKUP: &str = "KRS-E4307";
/// 格式名稱
pub const FORMAT: &str = "kairos.keys-backup/v1";
/// 金鑰備份檔的副檔名（接在資料庫備份檔的路徑後面）
pub const SUFFIX: &str = ".keys";
/// 備份的金鑰檔（資料目錄下）
pub const KEY_FILES: [&str; 3] = [
    crate::alerts::secrets::KEY_FILE,
    crate::users::secrets::KEY_FILE,
    crate::audit::KEY_FILE,
];
/// 備份密碼的最短長度（字元）
pub const MIN_PASSWORD_CHARS: usize = 12;
/// 備份密碼的最長長度（bytes；從檔案讀取時避免讀入整個大檔）
pub const MAX_PASSWORD_BYTES: usize = 1024;

/// Argon2id 的參數
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

/// 寫入時的預設（OWASP 對 Argon2id 的建議值以上；備份與還原時才計算一次）
pub const DEFAULT_KDF: KdfParams = KdfParams {
    m_kib: 64 * 1024,
    t: 3,
    p: 1,
};
/// 讀取時接受的上限
const MAX_M_KIB: u32 = 256 * 1024;
const MAX_T: u32 = 20;
const MAX_P: u32 = 8;

/// 備份密碼（記憶體中用完即清除）
pub struct Password(Zeroizing<String>);

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Password(…)")
    }
}

impl Password {
    /// 檢查長度；去掉結尾的一個換行（`\n` 或 `\r\n`），其餘原樣（含空白）
    pub fn new(text: Zeroizing<String>) -> Result<Self, String> {
        let mut s = text;
        if s.ends_with('\n') {
            s.pop();
            if s.ends_with('\r') {
                s.pop();
            }
        }
        if s.chars().count() < MIN_PASSWORD_CHARS {
            return Err(format!("備份密碼至少要 {MIN_PASSWORD_CHARS} 個字元"));
        }
        if s.trim().is_empty() || s.contains('\n') || s.contains('\0') {
            return Err("備份密碼必須是一行文字（不能只有空白、不能含換行或 NUL）".to_string());
        }
        Ok(Self(s))
    }

    /// 從檔案讀取（第一行之後不能有其他內容）
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("無法讀取密碼檔 {}：{e}", path.display()))?;
        if !meta.is_file() {
            return Err(format!("密碼檔 {} 不是一般檔案", path.display()));
        }
        Self::from_reader(
            std::fs::File::open(path)
                .map_err(|e| format!("無法讀取密碼檔 {}：{e}", path.display()))?,
        )
    }

    /// 從 stdin 等串流讀取
    pub fn from_reader(r: impl Read) -> Result<Self, String> {
        let mut buf = Zeroizing::new(Vec::new());
        r.take(MAX_PASSWORD_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(|e| format!("無法讀取備份密碼：{e}"))?;
        if buf.len() > MAX_PASSWORD_BYTES {
            return Err(format!("備份密碼超過 {MAX_PASSWORD_BYTES} bytes"));
        }
        let text = std::str::from_utf8(&buf).map_err(|_| "備份密碼不是 UTF-8 文字".to_string())?;
        Self::new(Zeroizing::new(text.to_string()))
    }
}

fn derive(password: &Password, salt: &[u8], p: KdfParams) -> Result<Zeroizing<[u8; 32]>, String> {
    let params =
        Params::new(p.m_kib, p.t, p.p, Some(32)).map_err(|e| format!("Argon2 參數無效：{e}"))?;
    let mut key = Zeroizing::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.0.as_bytes(), salt, &mut key[..])
        .map_err(|e| format!("無法由備份密碼衍生金鑰：{e}"))?;
    Ok(key)
}

/// 附加資料：`format`、`kdf`、`cipher` 的正規 JSON（serde_json 的 Map 依鍵排序）
fn aad(kdf: &Value) -> Vec<u8> {
    json!({ "format": FORMAT, "kdf": kdf, "cipher": "aes-256-gcm" })
        .to_string()
        .into_bytes()
}

/// 一組金鑰檔：(名稱, 內容)
pub type KeySet = Vec<(String, Zeroizing<Vec<u8>>)>;

fn encode_plain(files: &KeySet) -> Zeroizing<Vec<u8>> {
    let mut out = Zeroizing::new(Vec::new());
    out.push(files.len() as u8);
    for (name, data) in files {
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(data);
    }
    out
}

fn decode_plain(b: &[u8]) -> Option<KeySet> {
    let (&n, mut rest) = b.split_first()?;
    let mut files = Vec::new();
    for _ in 0..n {
        let (&nl, r) = rest.split_first()?;
        if r.len() < usize::from(nl) + 2 {
            return None;
        }
        let name = std::str::from_utf8(&r[..usize::from(nl)]).ok()?.to_string();
        let len = usize::from(u16::from_be_bytes([
            r[usize::from(nl)],
            r[usize::from(nl) + 1],
        ]));
        let r = &r[usize::from(nl) + 2..];
        if r.len() < len {
            return None;
        }
        files.push((name, Zeroizing::new(r[..len].to_vec())));
        rest = &r[len..];
    }
    rest.is_empty().then_some(files)
}

/// 讀取資料目錄中存在的金鑰檔；回傳（金鑰檔, 不存在的檔名）
pub fn read_keys(data_dir: &Path) -> Result<(KeySet, Vec<&'static str>), Error> {
    let mut files = Vec::new();
    let mut missing = Vec::new();
    for name in KEY_FILES {
        let path = data_dir.join(name);
        let mut buf = Zeroizing::new(Vec::new());
        match std::fs::File::open(&path) {
            Ok(mut f) => {
                f.read_to_end(&mut buf).map_err(|e| {
                    Error::new(e.kind(), format!("無法讀取 {}: {e}", path.display()))
                })?;
                if buf.len() != 32 {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "{} 的長度不是 32 bytes（檔案可能已損毀），不備份",
                            path.display()
                        ),
                    ));
                }
                files.push((name.to_string(), buf));
            }
            Err(e) if e.kind() == ErrorKind::NotFound => missing.push(name),
            Err(e) => {
                return Err(Error::new(
                    e.kind(),
                    format!("無法讀取 {}: {e}", path.display()),
                ));
            }
        }
    }
    Ok((files, missing))
}

/// 加密成備份檔的內容
pub fn seal(files: &KeySet, password: &Password, kdf: KdfParams) -> Result<String, String> {
    let rng = SystemRandom::new();
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill(&mut salt)
        .and_then(|_| rng.fill(&mut nonce))
        .map_err(|_| "無法取得隨機數（作業系統的 CSPRNG）".to_string())?;
    let kdf_json = json!({
        "alg": "argon2id", "m_kib": kdf.m_kib, "t": kdf.t, "p": kdf.p, "salt": b64url_encode(&salt),
    });
    let key = derive(password, &salt, kdf)?;
    let sealing = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, &key[..]).map_err(|_| "無法建立 AES-256-GCM 金鑰")?,
    );
    let mut data = encode_plain(files);
    sealing
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad(&kdf_json)),
            &mut *data,
        )
        .map_err(|_| "加密失敗".to_string())?;
    Ok(json!({
        "format": FORMAT,
        "kdf": kdf_json,
        "cipher": "aes-256-gcm",
        "nonce": b64url_encode(&nonce),
        "ciphertext": b64url_encode(&data),
        "created_at": crate::storage::fmt_time(chrono::Utc::now()),
        "kairos_version": env!("CARGO_PKG_VERSION"),
    })
    .to_string()
        + "\n")
}

/// 解開備份檔的內容；密碼錯誤與內容被修改無法區分
pub fn open(text: &str, password: &Password) -> Result<KeySet, String> {
    const BAD: &str = "密碼錯誤，或檔案被修改、損毀";
    let v: Value = serde_json::from_str(text.trim()).map_err(|_| {
        "不是 kairos db backup --include-keys 產生的金鑰備份檔（不是 JSON）".to_string()
    })?;
    if v.get("format").and_then(Value::as_str) != Some(FORMAT)
        || v.get("cipher").and_then(Value::as_str) != Some("aes-256-gcm")
    {
        return Err(format!("不支援的金鑰備份格式（需要 {FORMAT}）"));
    }
    let kdf_json = v.get("kdf").cloned().unwrap_or(Value::Null);
    let num = |k: &str| {
        kdf_json
            .get(k)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    };
    let (Some(m_kib), Some(t), Some(p)) = (num("m_kib"), num("t"), num("p")) else {
        return Err(BAD.to_string());
    };
    if kdf_json.get("alg").and_then(Value::as_str) != Some("argon2id")
        || m_kib > MAX_M_KIB
        || t > MAX_T
        || p > MAX_P
    {
        return Err(BAD.to_string());
    }
    let b64 = |x: Option<&Value>| x.and_then(Value::as_str).and_then(b64url_decode);
    let (Some(salt), Some(nonce), Some(mut data)) = (
        b64(kdf_json.get("salt")),
        b64(v.get("nonce")),
        b64(v.get("ciphertext")),
    ) else {
        return Err(BAD.to_string());
    };
    let nonce = Nonce::try_assume_unique_for_key(&nonce).map_err(|_| BAD.to_string())?;
    let key = derive(password, &salt, KdfParams { m_kib, t, p }).map_err(|_| BAD.to_string())?;
    let opening = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key[..]).map_err(|_| BAD)?);
    let plain = opening
        .open_in_place(nonce, Aad::from(aad(&kdf_json)), &mut data)
        .map_err(|_| BAD.to_string())?;
    let files = decode_plain(plain);
    let len = plain.len();
    // 解密後的明文在 data 中：用完即清除
    zeroize::Zeroize::zeroize(&mut data[..len]);
    let files = files.ok_or_else(|| BAD.to_string())?;
    for (name, content) in &files {
        if !KEY_FILES.contains(&name.as_str()) || content.len() != 32 {
            return Err(format!("金鑰備份中有不認得的檔案或長度不符：{name}"));
        }
    }
    Ok(files)
}

/// 寫出新檔案（不覆寫；0600）
pub fn write_new_private(path: &Path, content: &[u8]) -> Result<(), Error> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| match e.kind() {
        ErrorKind::AlreadyExists => Error::new(
            ErrorKind::AlreadyExists,
            format!("{} 已存在，不覆寫", path.display()),
        ),
        kind => Error::new(kind, format!("無法建立 {}: {e}", path.display())),
    })?;
    let r = f.write_all(content).and_then(|_| f.sync_all());
    if let Err(e) = r {
        drop(f);
        let _ = std::fs::remove_file(path);
        return Err(Error::new(
            e.kind(),
            format!("無法寫入 {}: {e}", path.display()),
        ));
    }
    Ok(())
}

/// 一個金鑰檔的還原結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restored {
    /// 寫入（原本不存在）
    Written,
    /// 與現有的檔案相同，未變更
    Unchanged,
    /// 取代了不同的檔案；舊檔改名保留在這裡
    Replaced(PathBuf),
}

/// 寫回資料目錄（0600，Unix 上擁有者與資料目錄相同）。與現有檔案不同時，沒有 `overwrite` 就
/// 拒絕（不寫入任何一個檔案）；有 `overwrite` 時舊檔改名為 `<名稱>.replaced-<時間>` 保留
pub fn restore(
    data_dir: &Path,
    files: &KeySet,
    overwrite: bool,
) -> Result<Vec<(String, Restored)>, Error> {
    // 先檢查全部，再寫入：不會只還原一半
    let mut plan = Vec::new();
    for (name, content) in files {
        let path = data_dir.join(name);
        match std::fs::read(&path) {
            Ok(old) => {
                let same = old.len() == content.len()
                    && bool::from(subtle::ConstantTimeEq::ct_eq(&old[..], &content[..]));
                let mut old = Zeroizing::new(old);
                zeroize::Zeroize::zeroize(&mut *old);
                if same {
                    plan.push((name.clone(), path, false, true));
                } else if overwrite {
                    plan.push((name.clone(), path, true, false));
                } else {
                    return Err(Error::new(
                        ErrorKind::AlreadyExists,
                        format!(
                            "{} 已存在且內容不同（資料庫可能是用它加密的）；確認要以備份取代時加 --overwrite（舊檔會改名保留）",
                            path.display()
                        ),
                    ));
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                plan.push((name.clone(), path, false, false));
            }
            Err(e) => {
                return Err(Error::new(
                    e.kind(),
                    format!("無法讀取 {}: {e}", path.display()),
                ));
            }
        }
    }
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut out = Vec::new();
    for ((name, path, replace, same), (_, content)) in plan.into_iter().zip(files) {
        if same {
            out.push((name, Restored::Unchanged));
            continue;
        }
        let tmp = data_dir.join(format!(".{name}.restore-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        write_new_private(&tmp, content)?;
        match_owner(data_dir, &tmp);
        let result = if replace {
            let kept = data_dir.join(format!("{name}.replaced-{stamp}"));
            std::fs::rename(&path, &kept)
                .and_then(|_| std::fs::rename(&tmp, &path))
                .map(|_| Restored::Replaced(kept))
        } else {
            std::fs::rename(&tmp, &path).map(|_| Restored::Written)
        };
        match result {
            Ok(r) => {
                crate::users::secrets::restrict_0600(&path);
                out.push((name, r));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(Error::new(
                    e.kind(),
                    format!("無法寫入 {}: {e}", path.display()),
                ));
            }
        }
    }
    sync_dir(data_dir);
    Ok(out)
}

/// 以 root 執行時，寫回的檔案改為資料目錄的擁有者（否則服務帳號讀不到 0600 的檔案）
fn match_owner(data_dir: &Path, path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(d), Ok(f)) = (std::fs::metadata(data_dir), std::fs::metadata(path))
            && (d.uid() != f.uid() || d.gid() != f.gid())
            && let Err(e) = std::os::unix::fs::chown(path, Some(d.uid()), Some(d.gid()))
        {
            log::warn!(
                "無法把 {} 的擁有者改為資料目錄的擁有者（{e}）；請以服務帳號執行（sudo -u kairos）",
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = (data_dir, path);
}

fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// 資料庫備份檔對應的金鑰備份檔路徑（`<備份檔>.keys`）
pub fn keys_path(db_backup: &Path) -> PathBuf {
    let mut s = db_backup.as_os_str().to_os_string();
    s.push(SUFFIX);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 測試用的低成本參數（正式寫入一律 [`DEFAULT_KDF`]）
    const FAST: KdfParams = KdfParams {
        m_kib: 64,
        t: 1,
        p: 1,
    };

    fn pw(s: &str) -> Password {
        Password::new(Zeroizing::new(s.to_string())).unwrap()
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kairos-keys-{tag}-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn keyset() -> KeySet {
        vec![
            ("alerts-secrets.key".into(), Zeroizing::new(vec![1u8; 32])),
            ("console.key".into(), Zeroizing::new(vec![2u8; 32])),
        ]
    }

    #[test]
    fn passwords_are_checked_and_one_trailing_newline_is_removed() {
        assert!(Password::new(Zeroizing::new("short".into())).is_err());
        assert!(Password::new(Zeroizing::new(" ".repeat(20))).is_err());
        assert!(Password::new(Zeroizing::new("two\nlines-xxxxxxxx".into())).is_err());
        let p = Password::from_reader(&b"correct horse battery\r\n"[..]).unwrap();
        assert_eq!(&*p.0, "correct horse battery");
        assert!(Password::from_reader(&vec![b'a'; MAX_PASSWORD_BYTES + 1][..]).is_err());
        assert_eq!(format!("{p:?}"), "Password(…)");
    }

    #[test]
    fn round_trip_wrong_password_and_tampering() {
        let text = seal(&keyset(), &pw("correct horse battery"), FAST).unwrap();
        // 檔案中沒有明文的金鑰
        assert!(!text.contains(&b64url_encode(&[1u8; 32])));
        let files = open(&text, &pw("correct horse battery")).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(&files[1].1[..], &[2u8; 32]);
        // 錯誤的密碼
        let e = open(&text, &pw("wrong horse battery")).unwrap_err();
        assert!(e.contains("密碼錯誤"), "{e}");
        // 竄改：密文、nonce、鹽、KDF 參數都會使解密失敗
        let v: Value = serde_json::from_str(&text).unwrap();
        let tamper = |f: &dyn Fn(&mut Value)| {
            let mut x = v.clone();
            f(&mut x);
            open(&x.to_string(), &pw("correct horse battery")).unwrap_err()
        };
        tamper(&|x| {
            let c = x["ciphertext"].as_str().unwrap();
            let mut b = b64url_decode(c).unwrap();
            b[3] ^= 1;
            x["ciphertext"] = json!(b64url_encode(&b));
        });
        tamper(&|x| x["nonce"] = json!(b64url_encode(&[0u8; 12])));
        tamper(&|x| x["kdf"]["salt"] = json!(b64url_encode(&[0u8; 16])));
        tamper(&|x| x["kdf"]["t"] = json!(2));
        // 被改成要求極大記憶體：直接拒絕，不計算
        tamper(&|x| x["kdf"]["m_kib"] = json!(64 * 1024 * 1024));
        assert!(
            open("not json", &pw("correct horse battery"))
                .unwrap_err()
                .contains("不是 JSON")
        );
        let mut other = v.clone();
        other["format"] = json!("other/v9");
        assert!(
            open(&other.to_string(), &pw("correct horse battery"))
                .unwrap_err()
                .contains("不支援")
        );
    }

    #[test]
    fn restore_writes_private_files_and_refuses_to_clobber_different_keys() {
        let dir = temp("restore");
        let files = keyset();
        // 第一次：寫入，0600
        let r = restore(&dir, &files, false).unwrap();
        assert!(r.iter().all(|(_, x)| *x == Restored::Written), "{r:?}");
        assert_eq!(
            std::fs::read(dir.join("console.key")).unwrap(),
            vec![2u8; 32]
        );
        #[cfg(unix)]
        for name in ["alerts-secrets.key", "console.key"] {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name} {mode:o}");
        }
        // 相同：不變
        let r = restore(&dir, &files, false).unwrap();
        assert!(r.iter().all(|(_, x)| *x == Restored::Unchanged));
        // 不同：沒有 --overwrite 時拒絕，而且一個都不寫
        std::fs::write(dir.join("console.key"), [9u8; 32]).unwrap();
        std::fs::remove_file(dir.join("alerts-secrets.key")).unwrap();
        let e = restore(&dir, &files, false).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::AlreadyExists);
        assert!(!dir.join("alerts-secrets.key").exists(), "不應只還原一半");
        // --overwrite：舊檔改名保留
        let r = restore(&dir, &files, true).unwrap();
        let kept = r
            .iter()
            .find_map(|(n, x)| match x {
                Restored::Replaced(p) if n == "console.key" => Some(p.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(std::fs::read(&kept).unwrap(), vec![9u8; 32]);
        assert_eq!(
            std::fs::read(dir.join("console.key")).unwrap(),
            vec![2u8; 32]
        );
        // 暫存檔不留下
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".restore-"))
            .collect();
        assert!(leftovers.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_keys_reports_missing_files_and_rejects_corrupt_ones() {
        let dir = temp("read");
        std::fs::write(dir.join("console.key"), [5u8; 32]).unwrap();
        let (files, missing) = read_keys(&dir).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(missing, vec!["alerts-secrets.key", "audit-hmac.key"]);
        std::fs::write(dir.join("audit-hmac.key"), b"short").unwrap();
        assert!(read_keys(&dir).is_err());
        assert_eq!(
            keys_path(Path::new("/b/k.db")),
            PathBuf::from("/b/k.db.keys")
        );
        // 已存在的備份檔不覆寫
        write_new_private(&dir.join("x.keys"), b"1").unwrap();
        assert!(write_new_private(&dir.join("x.keys"), b"2").is_err());
        assert_eq!(std::fs::read(dir.join("x.keys")).unwrap(), b"1");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
