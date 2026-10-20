//! `ssh-keygen -Y sign` 簽章（SSHSIG，OpenSSH 的 `PROTOCOL.sshsig`）的驗證與 `allowed_signers` 的解析
//! （REMOTE.md 第 4.2 節；RELEASING.md 第 3 節的簽章流程）。
//!
//! 以 ring 的 Ed25519 與 SHA-512 實作，**不呼叫系統的 `ssh-keygen`**（Windows、RHEL 8 的 OpenSSH 8.0
//! 沒有 `-Y verify`）。只支援發佈簽章實際使用的子集，其餘一律拒絕（fail closed）：
//!
//! - 金鑰只接受 `ssh-ed25519`；簽章的雜湊演算法只接受 `sha512`（`ssh-keygen -Y sign` 的預設）。
//! - `allowed_signers` 的選項只接受 `namespaces="…"`、`valid-after="…"`、`valid-before="…"`；
//!   `cert-authority` 或其他選項、principal 或 namespace 使用萬用字元（`*`、`?`、`!`）的行**整行拒絕**，
//!   原因記在 [`AllowedSigners::rejected`]（呼叫端寫進日誌）。
//! - 一行必須有 `namespaces=` 且包含驗證時要求的 namespace 才能用來驗證（`ssh-keygen` 允許省略，
//!   本實作依 REMOTE.md 第 4.2.2 節要求明確列出）。
//! - `valid-after`／`valid-before` 不帶 `Z` 時是「本機時間」：本 crate 不知道時區，因此取最保守的解讀
//!   （`valid-after` 視為最晚的 UTC＝加 12 小時、`valid-before` 視為最早的 UTC＝減 14 小時）。
//!   帶 `Z` 時為 UTC，與 `ssh-keygen` 完全相同。

use ring::digest;
use ring::signature::{ED25519, UnparsedPublicKey};
use std::fmt;

const MAGIC: &[u8; 6] = b"SSHSIG";
const SIG_VERSION: u32 = 1;
const KEY_TYPE_ED25519: &str = "ssh-ed25519";
const HASH_SHA512: &str = "sha512";
const ARMOR_BEGIN: &str = "-----BEGIN SSH SIGNATURE-----";
const ARMOR_END: &str = "-----END SSH SIGNATURE-----";

/// 簽章驗證失敗的原因（都對應 `PLS-E8401`／`KRS-E8401`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigError {
    /// 不是 SSHSIG（armor、base64 或二進位格式錯誤）
    Malformed(String),
    /// SSHSIG 版本不是 1
    UnsupportedVersion(u32),
    /// 簽章或公鑰不是 `ssh-ed25519`
    UnsupportedKeyType(String),
    /// 簽章的 namespace 不是要求的值
    WrongNamespace { expected: String, found: String },
    /// 雜湊演算法不是 `sha512`
    UnsupportedHashAlgorithm(String),
    /// Ed25519 簽章不符（檔案被修改，或簽章不是對這份內容產生）
    BadSignature,
    /// 簽章金鑰不在 `allowed_signers` 中，或該行的 principal 不是接受的名稱
    UnknownSigner,
    /// 金鑰在 `allowed_signers` 中，但該行的 `namespaces=` 沒有列出要求的 namespace
    NamespaceNotAllowed,
    /// 該行的 `valid-after` 還沒到
    NotYetValid,
    /// 該行的 `valid-before` 已過
    Expired,
}

impl fmt::Display for SigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SigError::Malformed(e) => write!(f, "malformed SSH signature: {e}"),
            SigError::UnsupportedVersion(v) => write!(f, "unsupported SSHSIG version {v}"),
            SigError::UnsupportedKeyType(t) => {
                write!(f, "unsupported key type {t:?} (only ssh-ed25519)")
            }
            SigError::WrongNamespace { expected, found } => {
                write!(f, "signature namespace is {found:?}, expected {expected:?}")
            }
            SigError::UnsupportedHashAlgorithm(h) => {
                write!(
                    f,
                    "unsupported signature hash algorithm {h:?} (only sha512)"
                )
            }
            SigError::BadSignature => f.write_str("signature does not match the signed file"),
            SigError::UnknownSigner => f.write_str(
                "signing key is not listed in allowed_signers for an accepted principal",
            ),
            SigError::NamespaceNotAllowed => {
                f.write_str("allowed_signers does not allow this key for the release namespace")
            }
            SigError::NotYetValid => f.write_str("signing key is not yet valid (valid-after)"),
            SigError::Expired => f.write_str("signing key has expired (valid-before)"),
        }
    }
}

impl std::error::Error for SigError {}

/// `allowed_signers` 中可用的一行
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerEntry {
    /// 行號（從 1 起算）
    pub line: usize,
    pub principals: Vec<String>,
    /// `namespaces="…"` 的內容；沒有時 `None`（此時不能用來驗證任何 namespace）
    pub namespaces: Option<Vec<String>>,
    /// Unix 秒（已依上面的規則換算成 UTC）
    pub valid_after: Option<i64>,
    pub valid_before: Option<i64>,
    /// Ed25519 公鑰
    pub public_key: [u8; 32],
}

impl SignerEntry {
    /// 與 `ssh-keygen -l` 相同的指紋：`SHA256:` 加公鑰 blob 的 SHA-256（base64、不補位）
    pub fn fingerprint(&self) -> String {
        key_fingerprint(&self.public_key)
    }
}

/// 被拒絕的一行（呼叫端記錄日誌）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedLine {
    pub line: usize,
    pub reason: String,
}

/// 解析後的 `allowed_signers`
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowedSigners {
    pub entries: Vec<SignerEntry>,
    pub rejected: Vec<RejectedLine>,
}

impl AllowedSigners {
    /// 解析 `ssh-keygen` 的 `allowed_signers` 格式（空行與 `#` 開頭的註解略過）。
    /// 不合規定的行放進 [`AllowedSigners::rejected`]，不影響其他行
    pub fn parse(text: &str) -> Self {
        let mut out = AllowedSigners::default();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match parse_line(i + 1, line) {
                Ok(entry) => out.entries.push(entry),
                Err(reason) => out.rejected.push(RejectedLine {
                    line: i + 1,
                    reason,
                }),
            }
        }
        out
    }

    /// 至少有一行可以用 `namespace` 驗證（沒有時視為沒有信任錨：`install.upgrade = "no_trust_anchor"`）
    pub fn has_usable_entry(&self, namespace: &str) -> bool {
        self.entries.iter().any(|e| {
            e.namespaces
                .as_ref()
                .is_some_and(|ns| ns.iter().any(|n| n == namespace))
        })
    }
}

/// 公鑰的指紋（`SHA256:` 加 base64、不補位），與 `ssh-keygen -l` 相同
pub fn key_fingerprint(public_key: &[u8; 32]) -> String {
    let blob = ed25519_key_blob(public_key);
    let d = digest::digest(&digest::SHA256, &blob);
    format!(
        "SHA256:{}",
        crate::b64::encode(d.as_ref()).trim_end_matches('=')
    )
}

fn ed25519_key_blob(public_key: &[u8; 32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(51);
    put_string(&mut blob, KEY_TYPE_ED25519.as_bytes());
    put_string(&mut blob, public_key);
    blob
}

fn put_string(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
}

/// 依序讀取 SSH 線上格式（RFC 4251）的欄位
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.0.len() < n {
            return Err("truncated data".to_string());
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn string(&mut self) -> Result<&'a [u8], String> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    fn text(&mut self) -> Result<&'a str, String> {
        std::str::from_utf8(self.string()?).map_err(|_| "field is not UTF-8".to_string())
    }

    fn finish(&self) -> Result<(), String> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err("trailing data".to_string())
        }
    }
}

/// 解析 `ssh-ed25519` 的公鑰 blob（`string "ssh-ed25519"`、`string 公鑰（32 bytes）`）
fn parse_ed25519_blob(blob: &[u8]) -> Result<[u8; 32], SigError> {
    let mut r = Reader(blob);
    let kind = r.text().map_err(SigError::Malformed)?;
    if kind != KEY_TYPE_ED25519 {
        return Err(SigError::UnsupportedKeyType(kind.to_string()));
    }
    let key = r.string().map_err(SigError::Malformed)?;
    r.finish().map_err(SigError::Malformed)?;
    key.try_into()
        .map_err(|_| SigError::Malformed("ed25519 public key must be 32 bytes".to_string()))
}

fn is_key_type_token(s: &str) -> bool {
    ["ssh-", "ecdsa-", "sk-", "rsa-sha2-"]
        .iter()
        .any(|p| s.starts_with(p))
}

/// 下一個以空白分隔的欄位；`quoted` 時雙引號內的空白不算分隔
fn next_field(s: &str, quoted: bool) -> (&str, &str) {
    let s = s.trim_start();
    let mut in_quote = false;
    for (i, c) in s.char_indices() {
        if quoted && c == '"' {
            in_quote = !in_quote;
        } else if c.is_whitespace() && !in_quote {
            return (&s[..i], &s[i..]);
        }
    }
    (s, "")
}

fn has_pattern_chars(s: &str) -> bool {
    s.contains(['*', '?', '!'])
}

fn parse_line(line_no: usize, line: &str) -> Result<SignerEntry, String> {
    let (principals, rest) = next_field(line, false);
    let principals: Vec<String> = principals.split(',').map(str::to_string).collect();
    if principals.iter().any(|p| p.is_empty()) {
        return Err("empty principal".to_string());
    }
    if principals.iter().any(|p| has_pattern_chars(p)) {
        return Err("principal patterns (*, ?, !) are not supported".to_string());
    }
    let (first, after) = next_field(rest, true);
    let (options, key_type, rest) = if is_key_type_token(first) {
        (None, first, after)
    } else {
        let (key_type, rest) = next_field(after, false);
        (Some(first), key_type, rest)
    };
    let (key_b64, _comment) = next_field(rest, false);
    if key_type.is_empty() || key_b64.is_empty() {
        return Err("missing key type or key".to_string());
    }
    if key_type != KEY_TYPE_ED25519 {
        return Err(format!(
            "unsupported key type {key_type:?} (only ssh-ed25519)"
        ));
    }
    let blob = crate::b64::decode(key_b64).map_err(|e| format!("invalid key: {e}"))?;
    let public_key = parse_ed25519_blob(&blob).map_err(|e| format!("invalid key: {e}"))?;

    let mut entry = SignerEntry {
        line: line_no,
        principals,
        namespaces: None,
        valid_after: None,
        valid_before: None,
        public_key,
    };
    if let Some(options) = options {
        let mut seen = Vec::new();
        for opt in split_options(options)? {
            let (name, value) = match opt.split_once('=') {
                Some((n, v)) => (n, Some(dequote(v)?)),
                None => (opt.as_str(), None),
            };
            let name = name.to_ascii_lowercase();
            if seen.contains(&name) {
                return Err(format!("duplicate option {name}"));
            }
            seen.push(name.clone());
            match (name.as_str(), value) {
                ("namespaces", Some(v)) => {
                    let ns: Vec<String> = v.split(',').map(str::to_string).collect();
                    if ns.iter().any(|n| n.is_empty() || has_pattern_chars(n)) {
                        return Err("namespaces must be a comma-separated list without patterns"
                            .to_string());
                    }
                    entry.namespaces = Some(ns);
                }
                ("valid-after", Some(v)) => entry.valid_after = Some(parse_time(&v, true)?),
                ("valid-before", Some(v)) => entry.valid_before = Some(parse_time(&v, false)?),
                ("cert-authority", _) => {
                    return Err("cert-authority is not supported".to_string());
                }
                (other, _) => return Err(format!("unsupported option {other:?}")),
            }
        }
    }
    Ok(entry)
}

/// 以逗號分開選項（雙引號內的逗號不算）
fn split_options(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quote = !in_quote;
                cur.push(c);
            }
            ',' if !in_quote => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    if in_quote {
        return Err("unterminated quote in options".to_string());
    }
    out.push(cur);
    if out.iter().any(String::is_empty) {
        return Err("empty option".to_string());
    }
    Ok(out)
}

fn dequote(v: &str) -> Result<String, String> {
    v.strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .filter(|v| !v.contains('"'))
        .map(str::to_string)
        .ok_or_else(|| "option values must be enclosed in double quotes".to_string())
}

/// `YYYYMMDD[HHMM[SS]][Z]` → Unix 秒。沒有 `Z` 時依「最保守」的時區換算（見模組說明）
fn parse_time(v: &str, is_after: bool) -> Result<i64, String> {
    let (digits, utc) = match v.strip_suffix(['Z', 'z']) {
        Some(d) => (d, true),
        None => (v, false),
    };
    let bad = || format!("invalid time {v:?} (expected YYYYMMDD[HHMM[SS]][Z])");
    if !digits.bytes().all(|b| b.is_ascii_digit()) || ![8, 12, 14].contains(&digits.len()) {
        return Err(bad());
    }
    let num = |a: usize, b: usize| digits.get(a..b).map_or(Ok(0), |s| s.parse::<u32>());
    let (y, mo, d) = (
        num(0, 4).map_err(|_| bad())?,
        num(4, 6).map_err(|_| bad())?,
        num(6, 8).map_err(|_| bad())?,
    );
    let (h, mi, s) = (
        num(8, 10).map_err(|_| bad())?,
        num(10, 12).map_err(|_| bad())?,
        num(12, 14).map_err(|_| bad())?,
    );
    let date = chrono::NaiveDate::from_ymd_opt(y as i32, mo, d).ok_or_else(bad)?;
    let t = date.and_hms_opt(h, mi, s).ok_or_else(bad)?;
    let secs = t.and_utc().timestamp();
    Ok(match (utc, is_after) {
        (true, _) => secs,
        (false, true) => secs + 12 * 3600,
        (false, false) => secs - 14 * 3600,
    })
}

/// 解析後的 SSHSIG
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshSignature {
    pub public_key: [u8; 32],
    pub namespace: String,
    pub hash_algorithm: String,
    reserved: Vec<u8>,
    signature: [u8; 64],
}

impl SshSignature {
    /// 解析 `-----BEGIN SSH SIGNATURE-----` 的 armor（`SHA256SUMS.sig` 的內容）
    pub fn parse(armored: &str) -> Result<Self, SigError> {
        let malformed = |e: &str| SigError::Malformed(e.to_string());
        let mut lines = armored.lines().map(str::trim).filter(|l| !l.is_empty());
        if lines.next() != Some(ARMOR_BEGIN) {
            return Err(malformed("missing BEGIN SSH SIGNATURE line"));
        }
        let mut body = String::new();
        let mut ended = false;
        for l in lines.by_ref() {
            if l == ARMOR_END {
                ended = true;
                break;
            }
            body.push_str(l);
        }
        if !ended || lines.next().is_some() {
            return Err(malformed("missing END SSH SIGNATURE line or trailing data"));
        }
        let blob = crate::b64::decode(&body).map_err(SigError::Malformed)?;
        let mut r = Reader(&blob);
        if r.take(6).map_err(SigError::Malformed)? != MAGIC {
            return Err(malformed("missing SSHSIG magic"));
        }
        let version = r.u32().map_err(SigError::Malformed)?;
        if version != SIG_VERSION {
            return Err(SigError::UnsupportedVersion(version));
        }
        let public_key = parse_ed25519_blob(r.string().map_err(SigError::Malformed)?)?;
        let namespace = r.text().map_err(SigError::Malformed)?.to_string();
        let reserved = r.string().map_err(SigError::Malformed)?.to_vec();
        let hash_algorithm = r.text().map_err(SigError::Malformed)?.to_string();
        let mut sig = Reader(r.string().map_err(SigError::Malformed)?);
        r.finish().map_err(SigError::Malformed)?;
        let kind = sig.text().map_err(SigError::Malformed)?;
        if kind != KEY_TYPE_ED25519 {
            return Err(SigError::UnsupportedKeyType(kind.to_string()));
        }
        let signature: [u8; 64] = sig
            .string()
            .map_err(SigError::Malformed)?
            .try_into()
            .map_err(|_| malformed("ed25519 signature must be 64 bytes"))?;
        sig.finish().map_err(SigError::Malformed)?;
        Ok(Self {
            public_key,
            namespace,
            hash_algorithm,
            reserved,
            signature,
        })
    }

    /// 簽章金鑰的指紋（`SHA256:…`）
    pub fn fingerprint(&self) -> String {
        key_fingerprint(&self.public_key)
    }

    /// 只驗證密碼學部分：namespace、雜湊演算法與 Ed25519 簽章（不看信任錨）
    pub fn verify_message(&self, namespace: &str, message: &[u8]) -> Result<(), SigError> {
        if self.namespace != namespace {
            return Err(SigError::WrongNamespace {
                expected: namespace.to_string(),
                found: self.namespace.clone(),
            });
        }
        if self.hash_algorithm != HASH_SHA512 {
            return Err(SigError::UnsupportedHashAlgorithm(
                self.hash_algorithm.clone(),
            ));
        }
        let mut signed = Vec::with_capacity(128);
        signed.extend_from_slice(MAGIC);
        put_string(&mut signed, self.namespace.as_bytes());
        put_string(&mut signed, &self.reserved);
        put_string(&mut signed, self.hash_algorithm.as_bytes());
        put_string(
            &mut signed,
            digest::digest(&digest::SHA512, message).as_ref(),
        );
        UnparsedPublicKey::new(&ED25519, &self.public_key)
            .verify(&signed, &self.signature)
            .map_err(|_| SigError::BadSignature)
    }
}

/// 驗證通過的簽署者
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSigner {
    /// 符合的 principal（`release@kairos-pulse` 或 `upgrade.extra_principals` 之一）
    pub principal: String,
    /// 金鑰指紋（`SHA256:…`）
    pub fingerprint: String,
    /// `allowed_signers` 中的行號
    pub line: usize,
}

/// 與 `ssh-keygen -Y verify -f <allowed_signers> -I <principal> -n <namespace> -s <sig> < <message>`
/// 相同的判斷（`principals` 為接受的名稱，任一符合即可；`now_unix` 為目前的 Unix 秒）
pub fn verify_sshsig(
    signers: &AllowedSigners,
    principals: &[&str],
    namespace: &str,
    message: &[u8],
    armored: &str,
    now_unix: i64,
) -> Result<VerifiedSigner, SigError> {
    let sig = SshSignature::parse(armored)?;
    sig.verify_message(namespace, message)?;
    let candidates: Vec<&SignerEntry> = signers
        .entries
        .iter()
        .filter(|e| e.public_key == sig.public_key)
        .filter(|e| {
            e.principals
                .iter()
                .any(|p| principals.contains(&p.as_str()))
        })
        .collect();
    if candidates.is_empty() {
        return Err(SigError::UnknownSigner);
    }
    let mut first_error = None;
    for e in candidates {
        let check = if !e
            .namespaces
            .as_ref()
            .is_some_and(|ns| ns.iter().any(|n| n == namespace))
        {
            Err(SigError::NamespaceNotAllowed)
        } else if e.valid_after.is_some_and(|t| now_unix < t) {
            Err(SigError::NotYetValid)
        } else if e.valid_before.is_some_and(|t| now_unix > t) {
            Err(SigError::Expired)
        } else {
            Ok(())
        };
        match check {
            Ok(()) => {
                let principal = e
                    .principals
                    .iter()
                    .find(|p| principals.contains(&p.as_str()))
                    .cloned()
                    .unwrap_or_default();
                return Ok(VerifiedSigner {
                    principal,
                    fingerprint: e.fingerprint(),
                    line: e.line,
                });
            }
            Err(err) => {
                first_error.get_or_insert(err);
            }
        }
    }
    Err(first_error.unwrap_or(SigError::UnknownSigner))
}
