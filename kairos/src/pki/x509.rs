//! 憑證、CRL 的解析與 PEM（x509-parser；`crates/kairos-proto/ENROLL.md` 第 15.3 節）。
//!
//! 只取出 Kairos 需要的欄位：序號（大寫十六進位）、有效期間、SPKI、subject／issuer、SAN、
//! 基本限制、金鑰用途與 EKU。

use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::prelude::{FromDer, X509Certificate};

/// 憑證中 Kairos 用到的欄位
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    /// 序號：大寫十六進位，去掉 DER 正整數的前導 `00`
    pub serial: String,
    pub not_before_ms: i64,
    pub not_after_ms: i64,
    /// SubjectPublicKeyInfo（DER）
    pub spki_der: Vec<u8>,
    /// subject 與 issuer 的原始 DER（比對簽發者用）
    pub subject_raw: Vec<u8>,
    pub issuer_raw: Vec<u8>,
    /// 給人看的 subject（`CN=…, O=…`）
    pub subject_text: String,
    pub issuer_text: String,
    pub common_names: Vec<String>,
    pub dns_sans: Vec<String>,
    /// IP 位址的 SAN
    pub ip_sans: Vec<std::net::IpAddr>,
    /// DNS 以外的 SAN 數量（含 IP）
    pub other_sans: usize,
    pub is_ca: bool,
    pub path_len: Option<u32>,
    /// 有金鑰用途擴充欄位時的內容（沒有時 `None`）
    pub key_usage: Option<KeyUsageBits>,
    /// 有 EKU 時的 (clientAuth, serverAuth, anyExtendedKeyUsage)
    pub eku: Option<(bool, bool, bool)>,
}

/// 金鑰用途中 Kairos 檢查的位元
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyUsageBits {
    pub digital_signature: bool,
    pub key_cert_sign: bool,
    pub crl_sign: bool,
}

/// DER 正整數的序號 → 大寫十六進位（去掉前導的 `00`；全為 0 時為 `0`）
pub fn serial_hex(raw: &[u8]) -> String {
    let mut v = raw;
    while v.len() > 1 && v[0] == 0 {
        v = &v[1..];
    }
    v.iter().map(|b| format!("{b:02X}")).collect()
}

/// 大寫十六進位的序號 → bytes（不合法時 `None`）
pub fn serial_bytes(hex: &str) -> Option<Vec<u8>> {
    if hex.is_empty() || !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

/// 解析 DER 憑證（結尾不得有多餘資料）
pub fn parse_cert(der: &[u8]) -> Result<CertInfo, String> {
    let (rest, cert) =
        X509Certificate::from_der(der).map_err(|e| format!("不是有效的 X.509 憑證：{e}"))?;
    if !rest.is_empty() {
        return Err("憑證的 DER 結尾有多餘的資料".to_string());
    }
    info_of(&cert)
}

fn info_of(cert: &X509Certificate<'_>) -> Result<CertInfo, String> {
    let tbs = &cert.tbs_certificate;
    let mut dns_sans = Vec::new();
    let mut ip_sans = Vec::new();
    let mut other_sans = 0;
    let mut is_ca = false;
    let mut path_len = None;
    let mut key_usage = None;
    let mut eku = None;
    for ext in tbs.extensions() {
        match ext.parsed_extension() {
            ParsedExtension::SubjectAlternativeName(san) => {
                for name in &san.general_names {
                    match name {
                        GeneralName::DNSName(n) => dns_sans.push((*n).to_string()),
                        GeneralName::IPAddress(b) => {
                            other_sans += 1;
                            if let Ok(v4) = <[u8; 4]>::try_from(*b) {
                                ip_sans.push(std::net::IpAddr::from(v4));
                            } else if let Ok(v6) = <[u8; 16]>::try_from(*b) {
                                ip_sans.push(std::net::IpAddr::from(v6));
                            }
                        }
                        _ => other_sans += 1,
                    }
                }
            }
            ParsedExtension::BasicConstraints(bc) => {
                is_ca = bc.ca;
                path_len = bc.path_len_constraint;
            }
            ParsedExtension::KeyUsage(ku) => {
                key_usage = Some(KeyUsageBits {
                    digital_signature: ku.digital_signature(),
                    key_cert_sign: ku.key_cert_sign(),
                    crl_sign: ku.crl_sign(),
                });
            }
            ParsedExtension::ExtendedKeyUsage(e) => {
                eku = Some((e.client_auth, e.server_auth, e.any));
            }
            ParsedExtension::ParseError { error } => {
                return Err(format!("憑證的擴充欄位無法解析：{error}"));
            }
            _ => {}
        }
    }
    let common_names = tbs
        .subject()
        .iter_common_name()
        .filter_map(|a| a.as_str().ok().map(str::to_string))
        .collect();
    Ok(CertInfo {
        serial: serial_hex(tbs.raw_serial()),
        not_before_ms: tbs.validity().not_before.timestamp().saturating_mul(1000),
        not_after_ms: tbs.validity().not_after.timestamp().saturating_mul(1000),
        spki_der: tbs.public_key().raw.to_vec(),
        subject_raw: tbs.subject().as_raw().to_vec(),
        issuer_raw: tbs.issuer().as_raw().to_vec(),
        subject_text: tbs.subject().to_string(),
        issuer_text: tbs.issuer().to_string(),
        common_names,
        dns_sans,
        ip_sans,
        other_sans,
        is_ca,
        path_len,
        key_usage,
        eku,
    })
}

/// 以簽發者的 SPKI 驗證憑證的簽章（不檢查有效期間；過期續簽的身分證明用）
pub fn verify_cert_signature(cert_der: &[u8], issuer_cert_der: &[u8]) -> bool {
    let Ok((_, cert)) = X509Certificate::from_der(cert_der) else {
        return false;
    };
    let Ok((_, issuer)) = X509Certificate::from_der(issuer_cert_der) else {
        return false;
    };
    if cert.tbs_certificate.issuer().as_raw() != issuer.tbs_certificate.subject().as_raw() {
        return false;
    }
    cert.verify_signature(Some(issuer.public_key())).is_ok()
}

/// PEM 編碼（64 字元換行）
pub fn pem_encode(label: &str, der: &[u8]) -> String {
    let b64 = base64_std(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// 讀出 PEM 中所有指定標籤的區塊（DER）；`label` 為 `None` 時接受任何標籤
pub fn pem_decode_all(text: &str, label: Option<&str>) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("-----BEGIN ")
            .and_then(|r| r.strip_suffix("-----"))
        else {
            continue;
        };
        let begin = rest.to_string();
        let mut body = String::new();
        let mut ended = false;
        for l in lines.by_ref() {
            let l = l.trim();
            if let Some(end) = l
                .strip_prefix("-----END ")
                .and_then(|r| r.strip_suffix("-----"))
            {
                if end != begin {
                    return Err(format!("PEM 的 BEGIN {begin} 與 END {end} 不一致"));
                }
                ended = true;
                break;
            }
            body.push_str(l);
        }
        if !ended {
            return Err(format!("PEM 的 {begin} 區塊沒有結尾"));
        }
        if label.is_none_or(|want| want == begin) {
            let der = base64_std_decode(&body)
                .ok_or_else(|| format!("PEM 的 {begin} 區塊不是有效的 base64"))?;
            out.push((begin, der));
        }
    }
    Ok(out)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 標準 base64（含 `=`；PEM 與主控台上傳的檔案）
pub fn base64_std(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// 解碼標準 base64（忽略空白；接受結尾的 `=`）
pub fn base64_std_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0;
    let mut padding = false;
    for c in text.bytes().filter(|c| !c.is_ascii_whitespace()) {
        if c == b'=' {
            padding = true;
            continue;
        }
        if padding {
            return None;
        }
        let v = B64.iter().position(|&x| x == c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_hex_strips_leading_zero_and_round_trips() {
        assert_eq!(serial_hex(&[0x00, 0x8f, 0x01]), "8F01");
        assert_eq!(serial_hex(&[0x00]), "00");
        assert_eq!(serial_hex(&[0x5e, 0x0b]), "5E0B");
        assert_eq!(serial_bytes("5E0B"), Some(vec![0x5e, 0x0b]));
        assert_eq!(serial_bytes("5E0"), None);
        assert_eq!(serial_bytes("ZZ"), None);
    }

    #[test]
    fn base64_and_pem_round_trip() {
        for data in [
            &b""[..],
            b"a",
            b"ab",
            b"abc",
            b"abcd",
            &[0u8, 255, 128, 7, 9][..],
        ] {
            assert_eq!(base64_std_decode(&base64_std(data)).unwrap(), data);
        }
        assert_eq!(base64_std(b"Man"), "TWFu");
        assert_eq!(base64_std(b"Ma"), "TWE=");
        let der: Vec<u8> = (0..200u8).collect();
        let pem = pem_encode("CERTIFICATE", &der);
        assert!(pem.lines().all(|l| l.len() <= 64));
        let back = pem_decode_all(&pem, Some("CERTIFICATE")).unwrap();
        assert_eq!(back, vec![("CERTIFICATE".to_string(), der)]);
        assert!(pem_decode_all(&pem, Some("X509 CRL")).unwrap().is_empty());
        assert!(pem_decode_all("-----BEGIN A-----\nAAAA\n", None).is_err());
        assert!(pem_decode_all("-----BEGIN A-----\n!!\n-----END A-----\n", None).is_err());
    }
}
