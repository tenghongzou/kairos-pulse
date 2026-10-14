//! 通知管道的機密（SMTP 密碼、Webhook URL 與標頭、HMAC 金鑰、LINE token）：
//!
//! - **保存**：以 AES-256-GCM 加密後存入 `alert_channels.secrets`；金鑰在資料目錄的
//!   [`KEY_FILE`]（32 bytes，0600，第一次需要時建立）。`kairos db backup` 的備份檔只有密文，
//!   沒有金鑰檔就無法還原機密（還原到新主機時要一起複製金鑰檔，或重新輸入機密）。
//! - **顯示**：API 回應、CLI 與日誌只顯示遮蔽值（[`MASK`]；URL 只顯示 scheme 與主機），
//!   只能更換不能檢視（S11）。

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::Value;
use std::io::{Error, ErrorKind, Read, Write};
use std::path::Path;

/// 金鑰檔（資料目錄下）
pub const KEY_FILE: &str = "alerts-secrets.key";
/// 遮蔽值
pub const MASK: &str = "••••";
/// 密文格式的版本
const VERSION: u8 = 1;

/// 加解密
pub struct SecretBox {
    key: LessSafeKey,
}

impl std::fmt::Debug for SecretBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBox(..)")
    }
}

impl SecretBox {
    /// 讀取資料目錄的金鑰檔；不存在時建立（0600）
    pub fn open(data_dir: &Path) -> Result<Self, Error> {
        let path = data_dir.join(KEY_FILE);
        let mut bytes = Vec::new();
        match std::fs::File::open(&path) {
            Ok(mut f) => {
                f.read_to_end(&mut bytes)?;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let mut key = [0u8; 32];
                SystemRandom::new()
                    .fill(&mut key)
                    .map_err(|_| Error::other("無法產生隨機數"))?;
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                match options.open(&path) {
                    Ok(mut f) => {
                        f.write_all(&key)?;
                        f.sync_all()?;
                        bytes = key.to_vec();
                    }
                    // 同時有另一個行程（CLI）建立：改讀它建立的
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                        std::fs::File::open(&path)?.read_to_end(&mut bytes)?;
                    }
                    Err(e) => {
                        return Err(Error::new(
                            e.kind(),
                            format!("無法建立 {}: {e}", path.display()),
                        ));
                    }
                }
            }
            Err(e) => {
                return Err(Error::new(
                    e.kind(),
                    format!("無法讀取 {}: {e}", path.display()),
                ));
            }
        }
        Self::from_key(&bytes).map_err(|e| {
            Error::new(
                ErrorKind::InvalidData,
                format!("{} 無效：{e}", path.display()),
            )
        })
    }

    pub fn from_key(bytes: &[u8]) -> Result<Self, String> {
        let unbound = UnboundKey::new(&AES_256_GCM, bytes)
            .map_err(|_| "金鑰長度必須是 32 bytes".to_string())?;
        Ok(Self {
            key: LessSafeKey::new(unbound),
        })
    }

    /// 加密（版本 1 bytes ＋ nonce 12 bytes ＋ 密文與 tag）
    pub fn seal(&self, value: &Value) -> Result<Vec<u8>, String> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| "無法產生隨機數".to_string())?;
        let mut data = serde_json::to_vec(value).map_err(|e| e.to_string())?;
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from([VERSION]),
                &mut data,
            )
            .map_err(|_| "加密失敗".to_string())?;
        let mut out = Vec::with_capacity(1 + NONCE_LEN + data.len());
        out.push(VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&data);
        Ok(out)
    }

    /// 解密
    pub fn unseal(&self, blob: &[u8]) -> Result<Value, String> {
        if blob.len() < 1 + NONCE_LEN || blob[0] != VERSION {
            return Err("機密的格式不正確".to_string());
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&blob[1..=NONCE_LEN]);
        let mut data = blob[1 + NONCE_LEN..].to_vec();
        let plain = self
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from([VERSION]),
                &mut data,
            )
            .map_err(|_| "無法解密（金鑰檔與資料庫不相符？）".to_string())?;
        serde_json::from_slice(plain).map_err(|e| e.to_string())
    }
}

/// URL 的遮蔽值：只留 scheme 與主機（`https://hooks.slack.com/…`），路徑與查詢字串可能含 token
pub fn mask_url(url: &str) -> String {
    let url = url.trim();
    let Some((scheme, rest)) = url.split_once("://") else {
        return MASK.to_string();
    };
    // 去掉 userinfo（user:pass@）
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    if host.is_empty() {
        return MASK.to_string();
    }
    let more = rest.len() > authority.len() || authority.contains('@');
    format!("{scheme}://{host}{}", if more { "/…" } else { "" })
}

/// 機密的遮蔽表示：字串為 [`MASK`]（URL 為 [`mask_url`]），物件逐欄遮蔽、保留欄位名稱
pub fn masked(secrets: &Value) -> Value {
    match secrets {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let m = match v {
                        Value::String(s) if k == "url" => Value::String(mask_url(s)),
                        Value::String(s) if s.is_empty() => Value::String(String::new()),
                        Value::Object(_) => masked(v),
                        Value::Null => Value::Null,
                        _ => Value::String(MASK.to_string()),
                    };
                    (k.clone(), m)
                })
                .collect(),
        ),
        Value::Null => Value::Null,
        _ => Value::String(MASK.to_string()),
    }
}

/// 從文字中移除機密（錯誤訊息可能帶有 URL；寫入日誌與發送紀錄前使用）
pub fn scrub(text: &str, secrets: &Value) -> String {
    let mut out = text.to_string();
    let mut values = Vec::new();
    collect_strings(secrets, &mut values);
    // 長的先換，避免短的值是長的值的一部分
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    for v in values {
        if v.len() >= 4 {
            out = out.replace(&v, MASK);
        }
    }
    out
}

fn collect_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            out.push(s.clone());
            // URL 的路徑與查詢字串也可能單獨出現在錯誤訊息中
            if let Some((_, rest)) = s.split_once("://")
                && let Some(p) = rest.find(['/', '?'])
            {
                out.push(rest[p..].to_string());
            }
        }
        Value::Object(m) => m.values().for_each(|x| collect_strings(x, out)),
        Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn seal_and_unseal_round_trip_and_reject_tampering() {
        let dir = std::env::temp_dir().join(format!(
            "kairos-secrets-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let b = SecretBox::open(&dir).unwrap();
        let secret = json!({"password": "hunter2", "url": "https://h.example/T0/B1/xyz"});
        let blob = b.seal(&secret).unwrap();
        assert!(!String::from_utf8_lossy(&blob).contains("hunter2"));
        assert_eq!(b.unseal(&blob).unwrap(), secret);
        // 同一個金鑰檔：再開一次也能解密
        let again = SecretBox::open(&dir).unwrap();
        assert_eq!(again.unseal(&blob).unwrap(), secret);
        let mut bad = blob.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(b.unseal(&bad).is_err());
        assert!(
            SecretBox::from_key(&[1u8; 32])
                .unwrap()
                .unseal(&blob)
                .is_err()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn masking_never_shows_the_secret() {
        assert_eq!(
            mask_url("https://hooks.slack.com/services/T0/B1/abc"),
            "https://hooks.slack.com/…"
        );
        assert_eq!(
            mask_url("https://user:pw@siem.local:8443/x?token=1"),
            "https://siem.local:8443/…"
        );
        assert_eq!(mask_url("https://siem.local"), "https://siem.local");
        assert_eq!(mask_url("not a url"), MASK);
        let m = masked(&json!({
            "url": "https://x.example/abc",
            "password": "p",
            "headers": {"Authorization": "Bearer s3cr3t"},
            "hmac_secret": "",
        }));
        assert_eq!(m["url"], "https://x.example/…");
        assert_eq!(m["password"], MASK);
        assert_eq!(m["headers"]["Authorization"], MASK);
        assert_eq!(m["hmac_secret"], "");
        let text = "POST https://x.example/abc?token=zzz9 failed: Bearer s3cr3t";
        let s = scrub(
            text,
            &json!({"url": "https://x.example/abc?token=zzz9", "h": {"a": "Bearer s3cr3t"}}),
        );
        assert!(!s.contains("zzz9") && !s.contains("s3cr3t"), "{s}");
    }
}
