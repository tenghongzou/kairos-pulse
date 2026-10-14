//! 註冊 token（`KRT1-XXXX-XXXX-XXXX-XXXX`）與安裝字串（`pulse-enroll:v1:…`）。
//!
//! 規格：ENROLL.md「註冊 token」「安裝字串」；設計包 `enrollment-ux.md` §3.1、§3.3、
//! `decisions/naming.md` §4.4。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

use super::crypto::{b64url_decode, b64url_encode, normalize_ca_pin, sha256};
use super::endpoint::normalize_endpoint;
use crate::{HostInfo, validate_host_info};

/// Crockford base32 的字母表（不含 I、L、O、U）
pub const CROCKFORD_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Crockford base32 字元的值：不分大小寫，`I`／`L` 視為 1，`O` 視為 0；`U` 與其他字元為 `None`
pub fn crockford_value(c: char) -> Option<u8> {
    let c = match c.to_ascii_uppercase() {
        'I' | 'L' => '1',
        'O' => '0',
        c => c,
    };
    CROCKFORD_ALPHABET
        .iter()
        .position(|&a| a as char == c)
        .map(|p| p as u8)
}

/// token 的版本前綴
pub const TOKEN_PREFIX: &str = "KRT1";
/// token 的秘密字元數（75 位元）
pub const TOKEN_SECRET_SYMBOLS: usize = 15;
/// token 雜湊（Kairos 只保存它）的用途字串
pub const TOKEN_HASH_DOMAIN: &str = "kairos-pulse enroll-token v1\n";

/// GF(2^5) 的既約多項式 x^5 + x^2 + 1（本原多項式，α = x 的階為 31）
const GF32_POLY: u8 = 0b10_0101;

/// GF(2^5) 乘法
pub(crate) fn gf32_mul(mut a: u8, mut b: u8) -> u8 {
    let mut r = 0;
    while b != 0 {
        if b & 1 != 0 {
            r ^= a;
        }
        b >>= 1;
        a <<= 1;
        if a & 0x20 != 0 {
            a ^= GF32_POLY;
        }
    }
    r
}

/// 檢查碼：`c = Σ α^(i+1) · v_i`（GF(2^5)，i = 0..14，v_i 為第 i 個秘密字元的值）。
/// 權重是相異的非零元素，所以任何一個字元打錯、任何兩個不同字元互換位置都一定改變檢查碼
pub fn token_check_symbol(values: &[u8; TOKEN_SECRET_SYMBOLS]) -> u8 {
    let mut weight = 1u8;
    let mut acc = 0u8;
    for &v in values {
        weight = gf32_mul(weight, 2);
        acc ^= gf32_mul(weight, v & 0x1f);
    }
    acc
}

/// token 格式錯誤（Pulse 在本機以 `PLS-E3001` 攔下，不送到伺服器）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenFormatError {
    /// 去掉連字號與空白後不是 20 個字元
    Length,
    /// 不是 `KRT1` 開頭（不認得的版本）
    Prefix,
    /// 含 Crockford base32 以外的字元
    Char,
    /// 檢查碼不符（打錯字）
    Check,
}

impl fmt::Display for TokenFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Length => "token must be KRT1 followed by 16 characters",
            Self::Prefix => "token must start with KRT1",
            Self::Char => "token contains characters outside Crockford base32",
            Self::Check => "token check character does not match (typo?)",
        })
    }
}

impl std::error::Error for TokenFormatError {}

/// 已驗證格式與檢查碼的註冊 token。`Debug` 不輸出秘密
#[derive(Clone, PartialEq, Eq)]
pub struct EnrollToken {
    /// 15 個秘密字元與 1 個檢查碼（Crockford 字母，大寫）
    body: [u8; 16],
}

impl fmt::Debug for EnrollToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EnrollToken({})", self.hint())
    }
}

impl EnrollToken {
    /// 解析使用者輸入：不分大小寫，連字號與空白可省略，`I`／`L` 視為 `1`、`O` 視為 `0`
    pub fn parse(input: &str) -> Result<Self, TokenFormatError> {
        let chars: Vec<char> = input
            .chars()
            .filter(|c| *c != '-' && !c.is_whitespace())
            .collect();
        if chars.len() != TOKEN_PREFIX.len() + 16 {
            return Err(TokenFormatError::Length);
        }
        let mut values = Vec::with_capacity(chars.len());
        for &c in &chars {
            values.push(crockford_value(c).ok_or(TokenFormatError::Char)?);
        }
        let prefix: String = values[..4]
            .iter()
            .map(|&v| CROCKFORD_ALPHABET[usize::from(v)] as char)
            .collect();
        if prefix != TOKEN_PREFIX {
            return Err(TokenFormatError::Prefix);
        }
        let mut secret = [0u8; TOKEN_SECRET_SYMBOLS];
        secret.copy_from_slice(&values[4..4 + TOKEN_SECRET_SYMBOLS]);
        if token_check_symbol(&secret) != values[19] {
            return Err(TokenFormatError::Check);
        }
        Ok(Self::from_values(&secret))
    }

    fn from_values(secret: &[u8; TOKEN_SECRET_SYMBOLS]) -> Self {
        let mut body = [0u8; 16];
        for (b, &v) in body.iter_mut().zip(secret) {
            *b = CROCKFORD_ALPHABET[usize::from(v & 0x1f)];
        }
        body[15] = CROCKFORD_ALPHABET[usize::from(token_check_symbol(secret))];
        Self { body }
    }

    /// 由 10 bytes 的隨機值產生 token（Kairos 以作業系統的 CSPRNG 取得）：取前 75 位元為 15 個字元
    pub fn from_random(bytes: &[u8; 10]) -> Self {
        let bits = bytes
            .iter()
            .fold(0u128, |acc, &b| (acc << 8) | u128::from(b));
        let mut secret = [0u8; TOKEN_SECRET_SYMBOLS];
        for (i, s) in secret.iter_mut().enumerate() {
            *s = ((bits >> (75 - 5 * i)) & 0x1f) as u8;
        }
        Self::from_values(&secret)
    }

    /// 正規寫法：`KRT1-XXXX-XXXX-XXXX-XXXX`（只在建立時完整顯示一次）
    pub fn canonical(&self) -> String {
        let s = self.body_str();
        format!(
            "{TOKEN_PREFIX}-{}-{}-{}-{}",
            &s[..4],
            &s[4..8],
            &s[8..12],
            &s[12..]
        )
    }

    /// 不含連字號：`KRT1XXXXXXXXXXXXXXXX`（雜湊的輸入）
    pub fn compact(&self) -> String {
        format!("{TOKEN_PREFIX}{}", self.body_str())
    }

    fn body_str(&self) -> String {
        self.body.iter().map(|&b| b as char).collect()
    }

    /// 遮蔽後的顯示：前 4 碼與後 4 碼，`KRT1-7G4Q…H6WN`（設計包 `cli-style.md` §10）
    pub fn hint(&self) -> String {
        let s = self.body_str();
        format!("{TOKEN_PREFIX}-{}…{}", &s[..4], &s[12..])
    }

    /// 秘密的前 4 碼（Pulse 的 `enrollment.json` 只記這 4 碼供診斷）
    pub fn prefix4(&self) -> String {
        self.body_str()[..4].to_string()
    }

    /// Kairos 保存與查詢用的雜湊：`SHA-256("kairos-pulse enroll-token v1\n" ‖ compact)`
    pub fn lookup_hash(&self) -> [u8; 32] {
        let mut input = TOKEN_HASH_DOMAIN.as_bytes().to_vec();
        input.extend_from_slice(self.compact().as_bytes());
        sha256(&input)
    }
}

/// 安裝字串的前綴（不含版本）
pub const INSTALL_STRING_SCHEME: &str = "pulse-enroll";
/// 目前產生的安裝字串版本
pub const INSTALL_STRING_VERSION: &str = "v1";

/// 安裝字串 v1 的內容（設計包 `decisions/naming.md` §4.4）。欄位固定為 `s`、`e`、`t`、`p`、`g`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallString {
    /// agent 連線位址 `host:port`（必填，含連接埠）
    pub s: String,
    /// 註冊端點 `host:port`（選填；省略時為 `s` 的主機加 8444）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e: Option<String>,
    /// 註冊 token（正規寫法）
    pub t: String,
    /// CA 指紋（`sha256:<64 個小寫十六進位>`）
    pub p: String,
    /// 預設標籤（選填）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub g: Option<Vec<String>>,
}

/// 安裝字串錯誤（Pulse 端 `PLS-E0002`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallStringError {
    /// 不是 `pulse-enroll:` 開頭
    Scheme,
    /// 不認得的版本（例如 `v2`）：請更新 Pulse
    UnknownVersion(String),
    /// base64url 或 JSON 格式錯誤
    Encoding,
    /// 含這個版本不認得的欄位：請更新 Pulse
    UnknownField(String),
    /// 欄位缺漏或值不合法
    Field(&'static str, String),
}

impl fmt::Display for InstallStringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scheme => f.write_str("install string must start with pulse-enroll:"),
            Self::UnknownVersion(v) => write!(f, "unsupported install string version {v:?}"),
            Self::Encoding => f.write_str("install string payload is not base64url JSON"),
            Self::UnknownField(k) => write!(f, "install string contains unknown field {k:?}"),
            Self::Field(k, why) => write!(f, "install string field {k}: {why}"),
        }
    }
}

impl std::error::Error for InstallStringError {}

impl InstallString {
    /// 檢查並正規化各欄位（端點轉小寫、token 轉正規寫法、CA 指紋轉小寫）
    pub fn normalized(&self) -> Result<Self, InstallStringError> {
        let s = normalize_endpoint(&self.s)
            .map_err(|e| InstallStringError::Field("s", e.to_string()))?;
        let e = match &self.e {
            Some(e) => Some(
                normalize_endpoint(e).map_err(|x| InstallStringError::Field("e", x.to_string()))?,
            ),
            None => None,
        };
        let t = EnrollToken::parse(&self.t)
            .map_err(|x| InstallStringError::Field("t", x.to_string()))?
            .canonical();
        let p =
            normalize_ca_pin(&self.p).map_err(|x| InstallStringError::Field("p", x.to_string()))?;
        if let Some(tags) = &self.g {
            let host = HostInfo {
                tags: Some(tags.clone()),
                ..HostInfo::default()
            };
            validate_host_info(&host).map_err(|x| InstallStringError::Field("g", x))?;
        }
        Ok(Self {
            s,
            e,
            t,
            p,
            g: self.g.clone().filter(|g| !g.is_empty()),
        })
    }

    /// 產生 `pulse-enroll:v1:<base64url(JSON)>`（不加 `=`；欄位依 s、e、t、p、g 的順序）
    pub fn encode(&self) -> Result<String, InstallStringError> {
        let n = self.normalized()?;
        let json = serde_json::to_string(&n).map_err(|_| InstallStringError::Encoding)?;
        Ok(format!(
            "{INSTALL_STRING_SCHEME}:{INSTALL_STRING_VERSION}:{}",
            b64url_encode(json.as_bytes())
        ))
    }

    /// 解析安裝字串並正規化。前後空白忽略；base64url 結尾的 `=` 容許（複製時常見），其餘嚴格
    pub fn decode(input: &str) -> Result<Self, InstallStringError> {
        let rest = input
            .trim()
            .strip_prefix(INSTALL_STRING_SCHEME)
            .and_then(|r| r.strip_prefix(':'))
            .ok_or(InstallStringError::Scheme)?;
        let (version, payload) = rest.split_once(':').ok_or(InstallStringError::Encoding)?;
        if version != INSTALL_STRING_VERSION {
            return Err(InstallStringError::UnknownVersion(version.to_string()));
        }
        let bytes = b64url_decode(payload.trim_end_matches('='))
            .map_err(|_| InstallStringError::Encoding)?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| InstallStringError::Encoding)?;
        let Value::Object(map) = &value else {
            return Err(InstallStringError::Encoding);
        };
        if let Some(k) = map
            .keys()
            .find(|k| !matches!(k.as_str(), "s" | "e" | "t" | "p" | "g"))
        {
            return Err(InstallStringError::UnknownField(k.clone()));
        }
        for k in ["s", "t", "p"] {
            if !map.get(k).is_some_and(Value::is_string) {
                return Err(InstallStringError::Field(k, "required string".to_string()));
            }
        }
        if map.get("e").is_some_and(|e| !e.is_string() && !e.is_null()) {
            return Err(InstallStringError::Field(
                "e",
                "must be a string".to_string(),
            ));
        }
        let parsed: Self = serde_json::from_value(value)
            .map_err(|e| InstallStringError::Field("g", e.to_string()))?;
        parsed.normalized()
    }
}
