//! 主控台金鑰（`<storage.data_dir>/console.key`，32 bytes 隨機值，權限 0600）：
//!
//! - CSRF token：`HMAC-SHA256(金鑰, "csrf" ‖ 工作階段的秘密)`（不需另外保存；工作階段結束即失效）；
//! - TOTP 秘密的加密（AES-256-GCM，以使用者 id 為附加資料，秘密不能被搬到另一個帳號）；
//! - 復原碼的 HMAC（資料庫外洩時無法離線猜出復原碼）。
//!
//! 金鑰只由 Kairos（主控台）在第一次需要時建立；遺失時已綁定的 TOTP 無法驗證，以
//! `kairos users reset-mfa` 重設後重新綁定。

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use std::io::{Error, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use crate::api::http::b64url_encode;

/// 金鑰檔名
pub const KEY_FILE: &str = "console.key";

/// 金鑰檔的權限收緊到 0600（Unix；其他人與群組都不能讀）
pub(crate) fn restrict_0600(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(path)
            && m.permissions().mode() & 0o077 != 0
        {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// 主控台金鑰
pub struct ConsoleKey {
    mac: hmac::Key,
    enc: LessSafeKey,
}

impl std::fmt::Debug for ConsoleKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConsoleKey(…)")
    }
}

impl ConsoleKey {
    pub fn from_bytes(key: &[u8; 32]) -> Result<Self, Error> {
        let mac = hmac::Key::new(hmac::HMAC_SHA256, key);
        // 加密用的金鑰由同一把金鑰以 HMAC 衍生（不同用途不共用同一把金鑰）
        let derived = hmac::sign(&mac, b"kairos-totp-encryption/v1");
        let enc = UnboundKey::new(&AES_256_GCM, derived.as_ref())
            .map_err(|_| Error::other("無法建立 AES-256-GCM 金鑰"))?;
        Ok(Self {
            mac,
            enc: LessSafeKey::new(enc),
        })
    }

    /// 讀取金鑰檔；不存在時建立（0600，只寫一次，不覆寫）
    pub fn load_or_create(data_dir: &Path) -> Result<Self, Error> {
        let path: PathBuf = data_dir.join(KEY_FILE);
        match std::fs::File::open(&path) {
            Ok(mut f) => {
                let mut buf = Vec::new();
                f.read_to_end(&mut buf)?;
                let key: [u8; 32] = buf.as_slice().try_into().map_err(|_| {
                    Error::new(
                        ErrorKind::InvalidData,
                        format!("{} 的長度不是 32 bytes（檔案可能已損毀）", path.display()),
                    )
                })?;
                restrict_0600(&path);
                Self::from_bytes(&key)
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                // 叢集模式下不自動建立（kairos/HA.md 第 11.1 節）
                crate::cluster::keys::check_autocreate(&path)?;
                let mut key = [0u8; 32];
                SystemRandom::new()
                    .fill(&mut key)
                    .map_err(|_| Error::other("無法取得隨機數（作業系統的 CSPRNG）"))?;
                let mut opts = std::fs::OpenOptions::new();
                opts.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    opts.mode(0o600);
                }
                match opts.open(&path) {
                    Ok(mut f) => {
                        f.write_all(&key)?;
                        f.sync_all()?;
                        Self::from_bytes(&key)
                    }
                    // 另一個執行緒或行程剛建立：改讀它的
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                        Self::load_or_create(data_dir)
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    fn tag(&self, label: &[u8], data: &[u8]) -> hmac::Tag {
        let mut ctx = hmac::Context::with_key(&self.mac);
        ctx.update(label);
        ctx.update(&[0]);
        ctx.update(data);
        ctx.sign()
    }

    /// 工作階段的 CSRF token（base64url）
    pub fn csrf_token(&self, session_secret: &str) -> String {
        b64url_encode(self.tag(b"csrf", session_secret.as_bytes()).as_ref())
    }

    /// 以常數時間比對 CSRF token
    pub fn csrf_matches(&self, session_secret: &str, presented: &str) -> bool {
        let expected = self.csrf_token(session_secret);
        expected.len() == presented.len()
            && bool::from(subtle::ConstantTimeEq::ct_eq(
                expected.as_bytes(),
                presented.as_bytes(),
            ))
    }

    /// 復原碼的 HMAC（綁定使用者 id）
    pub fn recovery_hash(&self, user_id: i64, code: &str) -> Vec<u8> {
        let mut data = user_id.to_be_bytes().to_vec();
        data.extend_from_slice(code.as_bytes());
        self.tag(b"recovery", &data).as_ref().to_vec()
    }

    /// 加密（nonce 12 bytes ‖ 密文 ‖ tag）；`aad` 為附加資料（使用者 id）
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, Error> {
        let mut nonce = [0u8; 12];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| Error::other("無法取得隨機數（作業系統的 CSPRNG）"))?;
        let mut buf = plaintext.to_vec();
        self.enc
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut buf,
            )
            .map_err(|_| Error::other("無法加密"))?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&buf);
        Ok(out)
    }

    /// 解密；金鑰不符、內容被修改或附加資料不同時為 `None`
    pub fn open(&self, sealed: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
        if sealed.len() < 12 + 16 {
            return None;
        }
        let (nonce, rest) = sealed.split_at(12);
        let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
        let mut buf = rest.to_vec();
        let plain = self
            .enc
            .open_in_place(nonce, Aad::from(aad), &mut buf)
            .ok()?;
        Some(plain.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_file_is_created_once_and_reused() {
        let dir = std::env::temp_dir().join(format!(
            "kairos-console-key-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let a = ConsoleKey::load_or_create(&dir).unwrap();
        let b = ConsoleKey::load_or_create(&dir).unwrap();
        assert_eq!(a.csrf_token("s"), b.csrf_token("s"));
        assert_eq!(std::fs::read(dir.join(KEY_FILE)).unwrap().len(), 32);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "{mode:o}");
        }
        // 損毀的金鑰檔：拒絕使用
        std::fs::write(dir.join(KEY_FILE), b"short").unwrap();
        assert!(ConsoleKey::load_or_create(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn csrf_seal_and_recovery() {
        let k = ConsoleKey::from_bytes(&[7u8; 32]).unwrap();
        let other = ConsoleKey::from_bytes(&[8u8; 32]).unwrap();
        let t = k.csrf_token("session-secret");
        assert_eq!(t.len(), 43);
        assert!(k.csrf_matches("session-secret", &t));
        assert!(!k.csrf_matches("other-session", &t));
        assert!(!k.csrf_matches("session-secret", &t[..42]));
        assert!(!other.csrf_matches("session-secret", &t));

        let sealed = k.seal(b"totp-secret", b"7").unwrap();
        assert!(!sealed.windows(11).any(|w| w == b"totp-secret"));
        assert_eq!(k.open(&sealed, b"7").as_deref(), Some(&b"totp-secret"[..]));
        // 搬到另一個使用者、換了金鑰、被修改：都無法解密
        assert_eq!(k.open(&sealed, b"8"), None);
        assert_eq!(other.open(&sealed, b"7"), None);
        let mut bad = sealed.clone();
        bad[20] ^= 1;
        assert_eq!(k.open(&bad, b"7"), None);
        assert_eq!(k.open(&sealed[..10], b"7"), None);

        assert_eq!(k.recovery_hash(1, "A"), k.recovery_hash(1, "A"));
        assert_ne!(k.recovery_hash(1, "A"), k.recovery_hash(2, "A"));
    }
}
