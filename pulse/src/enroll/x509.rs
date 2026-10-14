//! 最小的 X.509 解析：只讀註冊流程需要的欄位（序號、有效期間、SPKI、basicConstraints 的 CA 旗標、
//! SAN 的 DNS 名稱）。不驗證簽章（信任鏈由 rustls／webpki 驗證），格式不符時回傳 `None`、不 panic。
//!
//! 不引入 x509-parser：Pulse 只需要這幾個欄位，以既有的 DER 走訪寫法（同 `tls.rs`）即可；
//! 單元測試以 rcgen 產生的憑證比對結果。

use chrono::{DateTime, NaiveDate, Utc};
use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_rustls::rustls::pki_types::pem::PemObject;

const SEQUENCE: u8 = 0x30;
const INTEGER: u8 = 0x02;
const BOOLEAN: u8 = 0x01;
const OID: u8 = 0x06;
const OCTET_STRING: u8 = 0x04;
const VERSION: u8 = 0xa0;
const EXTENSIONS: u8 = 0xa3;
const DNS_NAME: u8 = 0x82;
/// 2.5.29.19 basicConstraints
const OID_BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
/// 2.5.29.17 subjectAltName
const OID_SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x11];

/// 憑證中註冊流程用到的欄位
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    /// 序號（去掉 DER 為表示正數而補的 0x00）
    pub serial: Vec<u8>,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    /// SubjectPublicKeyInfo 的完整 DER（含外層 SEQUENCE）
    pub spki: Vec<u8>,
    /// basicConstraints 的 cA；沒有這個擴充欄位時為 `false`
    pub is_ca: bool,
    /// SAN 中的 dNSName（依出現順序）
    pub dns_sans: Vec<String>,
    /// SAN 中 dNSName 以外的項目數
    pub other_sans: usize,
}

impl CertInfo {
    /// 序號的大寫十六進位（協定 `certificate.serial` 的寫法，不含 `:`）
    pub fn serial_hex(&self) -> String {
        self.serial.iter().map(|b| format!("{b:02X}")).collect()
    }

    /// 序號的顯示寫法（`10:02`，同 `pulse status`）
    pub fn serial_colon(&self) -> String {
        self.serial
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }
}

/// 一個 DER TLV：`(tag, 內容, 整個 TLV, 其後剩餘)`。只支援單一位元組的 tag 與至多 4 位元組的長度
fn tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let (bytes, rest) = rest.split_at(n);
        let len = bytes
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
        (len, rest)
    };
    if rest.len() < len {
        return None;
    }
    let header = input.len() - rest.len();
    let (content, after) = rest.split_at(len);
    Some((tag, content, &input[..header + len], after))
}

fn expect(input: &[u8], tag: u8) -> Option<(&[u8], &[u8], &[u8])> {
    let (t, content, full, rest) = tlv(input)?;
    (t == tag).then_some((content, full, rest))
}

/// UTCTime（`YYMMDDHHMMSSZ`，YY < 50 為 20YY）或 GeneralizedTime（`YYYYMMDDHHMMSSZ`）
fn der_time(tag: u8, content: &[u8]) -> Option<DateTime<Utc>> {
    const UTC_TIME: u8 = 0x17;
    const GENERALIZED_TIME: u8 = 0x18;
    let digits = std::str::from_utf8(content).ok()?.strip_suffix('Z')?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let num = |s: &str| s.parse::<u32>().ok();
    let (year, rest) = match (tag, digits.len()) {
        (UTC_TIME, 12) => {
            let yy = i32::try_from(num(&digits[..2])?).ok()?;
            (if yy < 50 { 2000 + yy } else { 1900 + yy }, &digits[2..])
        }
        (GENERALIZED_TIME, 14) => (i32::try_from(num(&digits[..4])?).ok()?, &digits[4..]),
        _ => return None,
    };
    let date = NaiveDate::from_ymd_opt(year, num(&rest[..2])?, num(&rest[2..4])?)?;
    let time = date.and_hms_opt(num(&rest[4..6])?, num(&rest[6..8])?, num(&rest[8..10])?)?;
    Some(time.and_utc())
}

/// 解析憑證（DER）
pub fn parse_cert(der: &[u8]) -> Option<CertInfo> {
    let (cert, _, _) = expect(der, SEQUENCE)?;
    let (tbs, _, _) = expect(cert, SEQUENCE)?;
    let (tag, content, _, rest) = tlv(tbs)?;
    let (serial, rest) = match tag {
        VERSION => {
            let (serial, _, rest) = expect(rest, INTEGER)?;
            (serial, rest)
        }
        INTEGER => (content, rest),
        _ => return None,
    };
    let serial = match serial {
        [0, tail @ ..] if !tail.is_empty() => tail,
        s => s,
    };
    let (_, _, rest) = expect(rest, SEQUENCE)?; // signature
    let (_, _, rest) = expect(rest, SEQUENCE)?; // issuer
    let (validity, _, rest) = expect(rest, SEQUENCE)?;
    let (tag, nb, _, vrest) = tlv(validity)?;
    let not_before = der_time(tag, nb)?;
    let (tag, na, _, _) = tlv(vrest)?;
    let not_after = der_time(tag, na)?;
    let (_, _, rest) = expect(rest, SEQUENCE)?; // subject
    let (_, spki, mut rest) = expect(rest, SEQUENCE)?;
    let mut info = CertInfo {
        serial: serial.to_vec(),
        not_before,
        not_after,
        spki: spki.to_vec(),
        is_ca: false,
        dns_sans: Vec::new(),
        other_sans: 0,
    };
    // issuerUniqueID [1]、subjectUniqueID [2]（略過），extensions [3]
    while let Some((tag, content, _, after)) = tlv(rest) {
        rest = after;
        if tag != EXTENSIONS {
            continue;
        }
        let (mut exts, _, _) = expect(content, SEQUENCE)?;
        while !exts.is_empty() {
            let (ext, _, after) = expect(exts, SEQUENCE)?;
            exts = after;
            let (oid, _, ext_rest) = expect(ext, OID)?;
            let ext_rest = match tlv(ext_rest)? {
                (BOOLEAN, _, _, r) => r,
                _ => ext_rest,
            };
            let (value, _, _) = expect(ext_rest, OCTET_STRING)?;
            if oid == OID_BASIC_CONSTRAINTS {
                let (bc, _, _) = expect(value, SEQUENCE)?;
                if let Some((BOOLEAN, flag, _, _)) = tlv(bc) {
                    info.is_ca = flag.first().is_some_and(|&b| b != 0);
                }
            } else if oid == OID_SUBJECT_ALT_NAME {
                let (mut names, _, _) = expect(value, SEQUENCE)?;
                while !names.is_empty() {
                    let (tag, content, _, after) = tlv(names)?;
                    names = after;
                    if tag == DNS_NAME {
                        info.dns_sans
                            .push(String::from_utf8_lossy(content).into_owned());
                    } else {
                        info.other_sans += 1;
                    }
                }
            }
        }
    }
    Some(info)
}

/// CSR（PKCS#10 DER）中的 SubjectPublicKeyInfo（完整 DER）
pub fn csr_spki(der: &[u8]) -> Option<Vec<u8>> {
    let (csr, _, _) = expect(der, SEQUENCE)?;
    let (info, _, _) = expect(csr, SEQUENCE)?;
    let (_, _, rest) = expect(info, INTEGER)?; // version
    let (_, _, rest) = expect(rest, SEQUENCE)?; // subject
    let (_, spki, _) = expect(rest, SEQUENCE)?;
    Some(spki.to_vec())
}

/// 讀取 PEM 文字中的所有憑證；一張都沒有或格式錯誤時回傳錯誤說明
pub fn pem_certs(pem: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let certs = CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("PEM 格式錯誤：{e}"))?;
    if certs.is_empty() {
        return Err("找不到憑證（需要 -----BEGIN CERTIFICATE----- 區塊）".to_string());
    }
    Ok(certs)
}

/// 憑證的 PEM（每行 64 個字元）
pub fn cert_pem(der: &[u8]) -> String {
    let b64 = base64_std(der);
    let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out
}

/// 標準 base64（含 `=`；PEM 用）
fn base64_std(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair,
        PKCS_ECDSA_P256_SHA256,
    };

    #[test]
    fn parses_fields_of_generated_certificates() {
        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name = DistinguishedName::new();
        ca.distinguished_name.push(DnType::CommonName, "Test CA");
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        let info = parse_cert(ca_cert.der()).unwrap();
        assert!(info.is_ca);
        assert_eq!(
            info.spki,
            kairos_proto::enroll::p256_spki_of_pkcs8(&ca_key.serialize_der()).unwrap()
        );
        assert!(info.dns_sans.is_empty());

        let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut leaf =
            CertificateParams::new(vec!["6f1c0e4a-1b2c-4d3e-9f00-112233445566".into()]).unwrap();
        leaf.serial_number = Some(rcgen::SerialNumber::from_slice(&[0x80, 0x01, 0x02]));
        leaf.not_before = rcgen::date_time_ymd(2026, 10, 11);
        leaf.not_after = rcgen::date_time_ymd(2027, 10, 11);
        let issuer = rcgen::Issuer::new(ca, &ca_key);
        let cert = leaf.signed_by(&leaf_key, &issuer).unwrap();
        let info = parse_cert(cert.der()).unwrap();
        assert!(!info.is_ca);
        assert_eq!(info.dns_sans, ["6f1c0e4a-1b2c-4d3e-9f00-112233445566"]);
        assert_eq!(info.serial_hex(), "800102");
        assert_eq!(info.serial_colon(), "80:01:02");
        assert_eq!(info.not_after.format("%Y-%m-%d").to_string(), "2027-10-11");
        // CSR 的 SPKI
        let csr = crate::enroll::keys::build_csr(
            &leaf_key.serialize_der(),
            "6f1c0e4a-1b2c-4d3e-9f00-112233445566",
        )
        .unwrap();
        assert_eq!(
            csr_spki(&csr).unwrap(),
            kairos_proto::enroll::p256_spki_of_pkcs8(&leaf_key.serialize_der()).unwrap()
        );
        // PEM 往返
        let pem = cert_pem(cert.der());
        assert_eq!(pem_certs(&pem).unwrap()[0].as_ref(), cert.der().as_ref());
        assert_eq!(pem.trim_end(), cert.pem().trim_end().replace("\r\n", "\n"));
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        assert!(parse_cert(b"").is_none());
        assert!(parse_cert(&[0x30, 0x82, 0xff]).is_none());
        assert!(pem_certs("not pem").is_err());
        for n in 0..64u8 {
            let junk: Vec<u8> = (0..n).map(|i| i.wrapping_mul(37) ^ n).collect();
            let _ = parse_cert(&junk);
        }
    }

    #[test]
    fn base64_matches_known_values() {
        assert_eq!(base64_std(b""), "");
        assert_eq!(base64_std(b"f"), "Zg==");
        assert_eq!(base64_std(b"fo"), "Zm8=");
        assert_eq!(base64_std(b"foo"), "Zm9v");
        assert_eq!(base64_std(b"foobar"), "Zm9vYmFy");
    }
}
