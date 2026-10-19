//! 標準 base64（RFC 4648 第 4 節，含 `=` 補位）的編碼與嚴格解碼。
//!
//! 用在遠端升級的分段下載（`upgrade_chunk.data`）、SSHSIG 的 ASCII armor 與 `allowed_signers` 的公鑰欄位。
//! 只有這幾十行，不另外引入 base64 套件。解碼只接受正規形式：長度是 4 的倍數、只有標準字母表、
//! `=` 只出現在結尾且最多兩個、補位前多出來的位元必須為 0（同一段資料只有一種寫法）。

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 標準 base64 編碼（含補位）
pub(crate) fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let idx = |shift: u32| char::from(ALPHABET[((n >> shift) & 0x3f) as usize]);
        out.push(idx(18));
        out.push(idx(12));
        out.push(if chunk.len() > 1 { idx(6) } else { '=' });
        out.push(if chunk.len() > 2 { idx(0) } else { '=' });
    }
    out
}

fn value(c: u8) -> Option<u32> {
    let v = match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    };
    Some(u32::from(v))
}

/// 嚴格的標準 base64 解碼；不合法時回傳錯誤說明
pub(crate) fn decode(text: &str) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err("base64 length must be a multiple of 4".to_string());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let quads = bytes.len() / 4;
    for (i, quad) in bytes.chunks(4).enumerate() {
        let last = i + 1 == quads;
        let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return Err("base64 padding is only allowed at the end (at most two '=')".to_string());
        }
        let mut n = 0u32;
        for &c in &quad[..4 - pad] {
            n = (n << 6)
                | value(c).ok_or("base64 contains a character outside the standard alphabet")?;
        }
        n <<= 6 * pad as u32;
        let b = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        // 補位前多出來的位元必須為 0（否則同一段資料有多種寫法）
        let unused = match pad {
            1 => b[2],
            2 => b[1] | b[2],
            _ => 0,
        };
        if unused != 0 {
            return Err("base64 has non-zero bits before the padding".to_string());
        }
        out.extend_from_slice(&b[..3 - pad]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors() {
        for (plain, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain.as_bytes()), enc);
            assert_eq!(decode(enc).unwrap(), plain.as_bytes());
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&all)).unwrap(), all);
        assert_eq!(decode("+/+/").unwrap(), [0xfb, 0xff, 0xbf]);
    }

    #[test]
    fn decoding_is_strict() {
        for bad in [
            "Zg", "Zg=", "Zg===", "Z===", "Zm9v\n", "Zm 9", "Zm9-", "Zm9_", "Zh==", "Zm9=Zm9v",
            "=Zm9", "Zm8=Zm8=",
        ] {
            assert!(decode(bad).is_err(), "{bad:?}");
        }
    }
}
