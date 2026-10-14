//! 註冊通道用到的編碼與密碼學基本操作：base64url、SHA-256、CA 指紋、公鑰指紋、申請編號、
//! ECDSA P-256 的簽章與驗證（ENROLL.md「編碼與雜湊」「簽章」）。
//!
//! 只依賴 ring（rustls 已使用的加密後端）；CSR 與憑證的 ASN.1 解析不在這裡（見 ENROLL.md
//! 「CSR 與憑證的套件」），函式一律以 SubjectPublicKeyInfo（SPKI）的 DER 為輸入。

use ring::digest;
use ring::rand::SystemRandom;
use ring::signature::{
    ECDSA_P256_SHA256_ASN1, ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair,
    UnparsedPublicKey,
};
use std::fmt;

use super::token::{CROCKFORD_ALPHABET, crockford_value};

const B64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url 編碼（RFC 4648 §5），**不加** `=`
pub fn b64url_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(B64URL_ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}

/// base64url 解碼失敗的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum B64Error {
    /// 含 base64url 以外的字元（包括 `=`、`+`、`/`、空白）
    InvalidChar,
    /// 長度除以 4 餘 1（不可能是合法的編碼）
    InvalidLength,
    /// 最後一個字元的未使用位元不是 0（非標準編碼；同一份資料只接受一種寫法）
    NonCanonical,
}

impl fmt::Display for B64Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidChar => "not base64url without padding",
            Self::InvalidLength => "invalid base64url length",
            Self::NonCanonical => "non-canonical base64url (unused bits must be zero)",
        })
    }
}

impl std::error::Error for B64Error {}

fn b64_value(c: u8) -> Option<u32> {
    Some(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'-' => 62,
        b'_' => 63,
        _ => return None,
    } as u32)
}

/// 嚴格的 base64url 解碼：不接受 `=`、不接受空白，未使用的位元必須為 0（每份資料只有一種
/// 合法寫法，雜湊與比對才不會因編碼不同而分歧）
pub fn b64url_decode(s: &str) -> Result<Vec<u8>, B64Error> {
    let bytes = s.as_bytes();
    if bytes.len() % 4 == 1 {
        return Err(B64Error::InvalidLength);
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= b64_value(c).ok_or(B64Error::InvalidChar)? << (18 - 6 * i);
        }
        let produced = chunk.len() - 1;
        // 最後一組不足 4 個字元時，多出的位元必須為 0
        let unused_mask = match chunk.len() {
            2 => 0xffff,
            3 => 0xff,
            _ => 0,
        };
        if n & unused_mask != 0 {
            return Err(B64Error::NonCanonical);
        }
        for i in 0..produced {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    Ok(out)
}

/// 嚴格解碼，並要求恰好 `len` bytes
pub fn b64url_decode_exact(s: &str, len: usize) -> Result<Vec<u8>, String> {
    let bytes = b64url_decode(s).map_err(|e| e.to_string())?;
    if bytes.len() == len {
        Ok(bytes)
    } else {
        Err(format!("must decode to {len} bytes (got {})", bytes.len()))
    }
}

/// SHA-256
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let d = digest::digest(&digest::SHA256, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

/// 小寫十六進位
pub fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 的小寫十六進位（例如過期續簽簽章內容中的 CSR 雜湊）
pub fn sha256_hex(data: &[u8]) -> String {
    hex_lower(&sha256(data))
}

/// 公鑰（SPKI DER）的指紋：`sha256:` 加 64 個小寫十六進位字元。主控台顯示「公鑰指紋前 8 碼」時
/// 取 `sha256:` 之後的前 8 個字元並轉大寫（[`spki_fingerprint_short`]）
pub fn spki_fingerprint(spki_der: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(spki_der))
}

/// 公鑰指紋前 8 碼（大寫），例：`3F9A1C07`
pub fn spki_fingerprint_short(spki_der: &[u8]) -> String {
    sha256_hex(spki_der)[..8].to_ascii_uppercase()
}

/// CA 指紋（安裝字串的 `p`、`--ca-pin`）：CA 憑證的 SPKI 的 SHA-256，`sha256:` 加 64 個小寫十六進位
/// 字元。與 [`spki_fingerprint`] 的算法相同，名稱分開是為了在程式中標明用途
pub fn ca_pin_from_spki(ca_spki_der: &[u8]) -> String {
    spki_fingerprint(ca_spki_der)
}

/// CA 指紋格式錯誤
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaPinError;

impl fmt::Display for CaPinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CA pin must be sha256: followed by 64 hexadecimal digits")
    }
}

impl std::error::Error for CaPinError {}

/// 把使用者輸入的 CA 指紋正規化為 `sha256:<64 個小寫十六進位>`：`sha256:` 不分大小寫、可省略；
/// 十六進位不分大小寫，可含空白與 `:`（從顯示格式或 OpenSSL 的 `AA:BB:…` 複製時）
pub fn normalize_ca_pin(input: &str) -> Result<String, CaPinError> {
    let trimmed = input.trim();
    let rest = match trimmed.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("sha256:") => &trimmed[7..],
        _ => trimmed,
    };
    let hex: String = rest
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(format!("sha256:{hex}"))
    } else {
        Err(CaPinError)
    }
}

/// CA 指紋的顯示格式（agent CLI、MSI、主控台三處一致；設計包 `enrollment-ux.md` §3.2）：
/// 大寫、每 4 碼一組、每 4 組一個大間隔（3 個空白），分兩行
pub fn ca_pin_display(pin: &str) -> Result<[String; 2], CaPinError> {
    let pin = normalize_ca_pin(pin)?;
    let hex = pin["sha256:".len()..].to_ascii_uppercase();
    let line = |half: &str| {
        let groups: Vec<&str> = (0..8).map(|i| &half[i * 4..i * 4 + 4]).collect();
        format!("{}   {}", groups[..4].join(" "), groups[4..].join(" "))
    };
    Ok([line(&hex[..32]), line(&hex[32..])])
}

/// 申請編號的用途字串（domain separation）
pub const REQUEST_CODE_DOMAIN: &str = "kairos-pulse request-code v1\n";

/// 申請編號（`request_code`）：給人以電話核對的 8 碼 Crockford base32，顯示為 `XXXX-XXXX`。
/// 由 CSR 公鑰衍生（[`request_code_from_spki`]），**不保證唯一**，不可當作識別碼或 API 路徑
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestCode([u8; 8]);

impl RequestCode {
    /// 解析人輸入的申請編號：不分大小寫，連字號與空白可省略，`I`／`L` 視為 `1`、`O` 視為 `0`
    pub fn parse(input: &str) -> Option<Self> {
        let mut out = [0u8; 8];
        let mut n = 0;
        for c in input.chars().filter(|c| *c != '-' && !c.is_whitespace()) {
            if n == 8 {
                return None;
            }
            out[n] = CROCKFORD_ALPHABET[usize::from(crockford_value(c)?)];
            n += 1;
        }
        (n == 8).then_some(Self(out))
    }

    /// 不含連字號的 8 個字元（搜尋索引用）
    pub fn compact(&self) -> String {
        self.0.iter().map(|&b| b as char).collect()
    }
}

impl fmt::Display for RequestCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.compact();
        write!(f, "{}-{}", &s[..4], &s[4..])
    }
}

impl serde::Serialize for RequestCode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for RequestCode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        // 線上一律是 XXXX-XXXX（大寫）；寬鬆的寫法只接受人輸入的場合（RequestCode::parse）
        let strict = s.len() == 9
            && s.as_bytes()[4] == b'-'
            && s.bytes()
                .enumerate()
                .all(|(i, b)| i == 4 || CROCKFORD_ALPHABET.contains(&b));
        match (strict, Self::parse(&s)) {
            (true, Some(code)) => Ok(code),
            _ => Err(serde::de::Error::custom(
                "request_code must be XXXX-XXXX in uppercase Crockford base32",
            )),
        }
    }
}

/// 由 CSR 的公鑰（SPKI DER）計算申請編號：
/// `SHA-256("kairos-pulse request-code v1\n" ‖ SPKI)` 的前 40 位元，每 5 位元一個 Crockford base32 字元
pub fn request_code_from_spki(spki_der: &[u8]) -> RequestCode {
    let mut input = Vec::with_capacity(REQUEST_CODE_DOMAIN.len() + spki_der.len());
    input.extend_from_slice(REQUEST_CODE_DOMAIN.as_bytes());
    input.extend_from_slice(spki_der);
    let h = sha256(&input);
    let bits = h[..5]
        .iter()
        .fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
    let mut out = [0u8; 8];
    for (i, o) in out.iter_mut().enumerate() {
        *o = CROCKFORD_ALPHABET[((bits >> (35 - 5 * i)) & 0x1f) as usize];
    }
    RequestCode(out)
}

/// P-256 公鑰的 SPKI DER 前綴（`id-ecPublicKey` ＋ `prime256v1`，未壓縮點 65 bytes）
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

/// 由 P-256 的 SPKI DER 取出未壓縮的公鑰點（65 bytes，`04` 開頭）；不是 P-256 未壓縮點時 `None`
pub fn p256_point_from_spki(spki_der: &[u8]) -> Option<&[u8]> {
    let point = spki_der.strip_prefix(&P256_SPKI_PREFIX[..])?;
    (point.len() == 65 && point[0] == 0x04).then_some(point)
}

/// 由 P-256 未壓縮公鑰點（65 bytes）組成 SPKI DER
pub fn p256_spki_from_point(point: &[u8]) -> Option<Vec<u8>> {
    if point.len() != 65 || point[0] != 0x04 {
        return None;
    }
    let mut spki = P256_SPKI_PREFIX.to_vec();
    spki.extend_from_slice(point);
    Some(spki)
}

/// 簽章或金鑰錯誤
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureError {
    /// 公鑰不是 P-256（v1 只支援 `ecdsa-p256-sha256`）
    UnsupportedKey,
    /// 簽章不是 base64url 或驗證失敗
    Invalid,
    /// 私鑰（PKCS#8）無法載入
    BadPrivateKey,
}

impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsupportedKey => "public key is not an uncompressed P-256 key",
            Self::Invalid => "signature verification failed",
            Self::BadPrivateKey => "private key is not a P-256 PKCS#8 document",
        })
    }
}

impl std::error::Error for SignatureError {}

/// 以 SPKI 驗證 `ecdsa-p256-sha256` 簽章（`signature` 為 base64url 的 ASN.1 DER）
pub fn verify_ecdsa_p256_sha256(
    spki_der: &[u8],
    message: &[u8],
    signature_b64url: &str,
) -> Result<(), SignatureError> {
    let point = p256_point_from_spki(spki_der).ok_or(SignatureError::UnsupportedKey)?;
    let sig = b64url_decode(signature_b64url).map_err(|_| SignatureError::Invalid)?;
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, point)
        .verify(message, &sig)
        .map_err(|_| SignatureError::Invalid)
}

/// 以 P-256 私鑰（PKCS#8 DER，需含公鑰，ring 與 OpenSSL 產生的格式皆可）簽章，回傳 base64url 的
/// ASN.1 DER。ECDSA 每次簽出的值不同，兩次簽章的位元組不會相同（重放快取因此以簽章內容為鍵）
pub fn sign_ecdsa_p256_sha256(pkcs8_der: &[u8], message: &[u8]) -> Result<String, SignatureError> {
    let rng = SystemRandom::new();
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8_der, &rng)
        .map_err(|_| SignatureError::BadPrivateKey)?;
    let sig = key
        .sign(&rng, message)
        .map_err(|_| SignatureError::Invalid)?;
    Ok(b64url_encode(sig.as_ref()))
}

/// P-256 私鑰（PKCS#8 DER）對應的 SPKI DER
pub fn p256_spki_of_pkcs8(pkcs8_der: &[u8]) -> Result<Vec<u8>, SignatureError> {
    let rng = SystemRandom::new();
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8_der, &rng)
        .map_err(|_| SignatureError::BadPrivateKey)?;
    p256_spki_from_point(key.public_key().as_ref()).ok_or(SignatureError::BadPrivateKey)
}
