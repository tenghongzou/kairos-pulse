//! TOTP 二階段驗證（RFC 6238：HMAC-SHA1、30 秒、6 位數；設計包 S01 第二步、S02、S16、S23）與
//! 10 組一次性的復原碼。
//!
//! - 秘密為 20 bytes 的 CSPRNG 隨機值；資料庫中以主控台金鑰加密保存（[`super::secrets::ConsoleKey::seal`]）。
//! - 驗證接受前後各 1 個時間步（±30 秒的時鐘偏差），同一個時間步只能用一次（防止重放）。
//! - 復原碼 10 組，每組 10 個字元（Crockford base32，約 50 位元），只在產生時顯示一次；資料庫保存
//!   以主控台金鑰計算的 HMAC。

use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};

/// 時間步長（秒）
pub const STEP_SECS: i64 = 30;
/// 位數
pub const DIGITS: u32 = 6;
/// 秘密的長度
pub const SECRET_BYTES: usize = 20;
/// 復原碼的組數
pub const RECOVERY_CODES: usize = 10;

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
/// 復原碼的字母表（Crockford base32：沒有 I、L、O、U）
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 新的秘密
pub fn generate_secret() -> Result<[u8; SECRET_BYTES], String> {
    let mut s = [0u8; SECRET_BYTES];
    SystemRandom::new()
        .fill(&mut s)
        .map_err(|_| "無法取得隨機數（作業系統的 CSPRNG）".to_string())?;
    Ok(s)
}

/// RFC 4648 base32（不補 `=`；驗證器 App 輸入用）
pub fn base32(data: &[u8]) -> String {
    let mut out = String::new();
    let mut buf: u64 = 0;
    let mut bits = 0;
    for &b in data {
        buf = (buf << 8) | u64::from(b);
        bits += 8;
        while bits >= 5 {
            out.push(B32[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// HOTP（RFC 4226）
pub fn hotp(secret: &[u8], counter: u64) -> u32 {
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret);
    let tag = hmac::sign(&key, &counter.to_be_bytes());
    let mac = tag.as_ref();
    let offset = (mac[mac.len() - 1] & 0x0f) as usize;
    let bin = (u32::from(mac[offset]) & 0x7f) << 24
        | u32::from(mac[offset + 1]) << 16
        | u32::from(mac[offset + 2]) << 8
        | u32::from(mac[offset + 3]);
    bin % 10u32.pow(DIGITS)
}

/// 某個時間的時間步
pub fn step_of(unix_secs: i64) -> i64 {
    unix_secs.div_euclid(STEP_SECS)
}

/// 驗證 6 位數的碼：接受目前時間步的前後各 1 步，且必須晚於上次使用的時間步。
/// 通過時回傳使用的時間步（呼叫端記錄下來，防止重放）
pub fn verify(secret: &[u8], code: &str, unix_secs: i64, last_step: Option<i64>) -> Option<i64> {
    let code = code.trim().replace([' ', '-'], "");
    if code.len() != DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let want: u32 = code.parse().ok()?;
    let now = step_of(unix_secs);
    let mut found = None;
    // 每個候選都計算並以常數時間比較，不提早結束
    for step in [now - 1, now, now + 1] {
        if step < 0 || last_step.is_some_and(|l| step <= l) {
            continue;
        }
        let got = hotp(secret, step as u64);
        let same: bool =
            subtle::ConstantTimeEq::ct_eq(&got.to_be_bytes()[..], &want.to_be_bytes()[..]).into();
        if same && found.is_none() {
            found = Some(step);
        }
    }
    found
}

/// 驗證器 App 的 `otpauth://` URI（QR code 的內容）
pub fn otpauth_uri(issuer: &str, account: &str, secret_b32: &str) -> String {
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect()
    };
    format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}",
        enc(issuer),
        enc(account),
        secret_b32,
        enc(issuer)
    )
}

/// 產生 10 組復原碼（`XXXXX-XXXXX`）
pub fn generate_recovery_codes() -> Result<Vec<String>, String> {
    let rng = SystemRandom::new();
    let mut out = Vec::with_capacity(RECOVERY_CODES);
    while out.len() < RECOVERY_CODES {
        let mut b = [0u8; 10];
        rng.fill(&mut b)
            .map_err(|_| "無法取得隨機數（作業系統的 CSPRNG）".to_string())?;
        let s: String = b
            .iter()
            .map(|x| CROCKFORD[(x & 31) as usize] as char)
            .collect();
        let code = format!("{}-{}", &s[..5], &s[5..]);
        if !out.contains(&code) {
            out.push(code);
        }
    }
    Ok(out)
}

/// 正規化使用者輸入的復原碼（不分大小寫、可省略連字號；Crockford 的 O→0、I／L→1）；格式不符時 `None`
pub fn normalize_recovery(input: &str) -> Option<String> {
    let s: String = input
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect();
    (s.len() == 10 && s.bytes().all(|b| CROCKFORD.contains(&b)))
        .then(|| format!("{}-{}", &s[..5], &s[5..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_and_rfc4226_vectors() {
        // RFC 4226 附錄 D
        let secret = b"12345678901234567890";
        let expected = [
            755224, 287082, 359152, 969429, 338314, 254676, 287922, 162583, 399871, 520489,
        ];
        for (i, e) in expected.iter().enumerate() {
            assert_eq!(hotp(secret, i as u64), *e, "counter {i}");
        }
        // RFC 6238 附錄 B（SHA1，8 位數的後 6 位）
        for (t, code8) in [
            (59_i64, 94_287_082_u32),
            (1_111_111_109, 7_081_804),
            (1_234_567_890, 89_005_924),
            (2_000_000_000, 69_279_037),
        ] {
            assert_eq!(hotp(secret, step_of(t) as u64), code8 % 1_000_000, "T={t}");
        }
        assert_eq!(
            base32(b"12345678901234567890"),
            "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"
        );
        assert_eq!(base32(b"f"), "MY");
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
    }

    #[test]
    fn verification_window_and_replay() {
        let secret = generate_secret().unwrap();
        let now = 1_760_000_000;
        let code = |t: i64| format!("{:06}", hotp(&secret, step_of(t) as u64));
        let step = verify(&secret, &code(now), now, None).unwrap();
        assert_eq!(step, step_of(now));
        // 同一步不能再用
        assert_eq!(verify(&secret, &code(now), now, Some(step)), None);
        // 前後各 1 步
        assert!(verify(&secret, &code(now - 30), now, None).is_some());
        assert!(verify(&secret, &code(now + 30), now, None).is_some());
        assert!(verify(&secret, &code(now - 90), now, None).is_none());
        // 格式
        assert!(verify(&secret, "12345", now, None).is_none());
        assert!(verify(&secret, "abcdef", now, None).is_none());
        let spaced = format!("{} {}", &code(now)[..3], &code(now)[3..]);
        assert!(verify(&secret, &spaced, now, None).is_some());
    }

    #[test]
    fn uri_and_recovery_codes() {
        let uri = otpauth_uri("Kairos", "admin@○○市政府", "ABC");
        assert!(
            uri.starts_with("otpauth://totp/Kairos:admin%40%E2%97%8B"),
            "{uri}"
        );
        assert!(uri.contains("secret=ABC&issuer=Kairos&algorithm=SHA1&digits=6&period=30"));
        let codes = generate_recovery_codes().unwrap();
        assert_eq!(codes.len(), RECOVERY_CODES);
        for c in &codes {
            assert_eq!(c.len(), 11);
            assert_eq!(normalize_recovery(c).as_deref(), Some(c.as_str()));
            assert_eq!(
                normalize_recovery(&c.to_lowercase().replace('-', "")).as_deref(),
                Some(c.as_str())
            );
        }
        assert_eq!(
            normalize_recovery("abcde-fghjk").as_deref(),
            Some("ABCDE-FGHJK")
        );
        assert_eq!(
            normalize_recovery("ABCDE-FGHO1").as_deref(),
            Some("ABCDE-FGH01")
        );
        assert_eq!(normalize_recovery("ABCDE-FGHU1"), None);
        assert_eq!(normalize_recovery("short"), None);
    }
}
