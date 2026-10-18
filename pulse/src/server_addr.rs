//! 伺服器的候選位址（`server.address` 加 `server.fallback_addresses`；`kairos/HA.md` 第 8 節）。
//!
//! - 候選清單＝`server.address:server.port`，接著 `fallback_addresses`（依設定順序），重複的只留第一個。
//! - `fallback_addresses` 的每一項是「主機」或「主機:埠」（省略埠時用 `server.port`）；IPv6 必須加 `[]`。
//!   依 ENROLL.md 的端點正規寫法解析（DNS 名稱轉小寫、IPv4 不接受前導 0、IPv6 為 RFC 5952 寫法）。
//! - 所有候選位址都以同一個 `server.domain` 驗證伺服器憑證（TLS 的名稱與實際連上的位址無關）。
//! - 環境變數 `PULSE__SERVER__FALLBACK_ADDRESSES` 以逗號分隔（去除前後空白，空字串表示沒有）。

use std::fmt;

use kairos_proto::enroll::normalize_endpoint;
use serde::{Deserialize, Deserializer};

/// `server.fallback_addresses` 的項目數上限
pub const MAX_FALLBACK_ADDRESSES: usize = 16;

/// 一個候選位址
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// 主機（DNS 名稱或 IP；IPv6 不含 `[]`），連線時才解析
    pub host: String,
    pub port: u16,
}

impl Candidate {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        let host: String = host.into();
        let host = host
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        Self { host, port }
    }

    /// 比對重複用的鍵（主機轉小寫、去掉結尾的 `.`）
    fn key(&self) -> (String, u16) {
        let h = self.host.trim_end_matches('.').to_ascii_lowercase();
        (h, self.port)
    }
}

/// `主機:埠`（IPv6 加上 `[]`）
impl fmt::Display for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

/// 解析 `fallback_addresses` 的一項；不合法時回傳說明（`PLS-E0201`）
pub fn parse_fallback(item: &str, default_port: u16) -> Result<Candidate, String> {
    let item = item.trim();
    let raw = if let Some(rest) = item.strip_prefix('[') {
        if rest.ends_with(']') {
            format!("{item}:{default_port}")
        } else {
            item.to_string()
        }
    } else {
        match item.matches(':').count() {
            0 => format!("{item}:{default_port}"),
            1 => item.to_string(),
            _ => {
                return Err(format!(
                    "「{item}」：IPv6 位址必須加上方括號，例如 [2001:db8::10] 或 [2001:db8::10]:8443"
                ));
            }
        }
    };
    let normalized =
        normalize_endpoint(&raw).map_err(|e| format!("「{item}」不是合法的位址：{e}"))?;
    let (host, port) = normalized
        .rsplit_once(':')
        .ok_or_else(|| format!("「{item}」不是合法的位址"))?;
    let port: u16 = port
        .parse()
        .map_err(|_| format!("「{item}」的連接埠不合法"))?;
    Ok(Candidate::new(host, port))
}

/// 候選清單：第一個一定是 `address:port`；之後是 `fallback`（不合法的項目略過，設定驗證時已拒絕）
pub fn candidates(address: &str, port: u16, fallback: &[String]) -> Vec<Candidate> {
    let mut out = vec![Candidate::new(address, port)];
    for item in fallback.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if let Ok(c) = parse_fallback(item, port)
            && !out.iter().any(|o| o.key() == c.key())
        {
            out.push(c);
        }
    }
    out
}

/// 驗證 `fallback_addresses`：最多 [`MAX_FALLBACK_ADDRESSES`] 項，每一項都能解析
pub fn validate_fallback(fallback: &[String], default_port: u16) -> Result<(), String> {
    let items: Vec<&str> = fallback
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if items.len() > MAX_FALLBACK_ADDRESSES {
        return Err(format!(
            "server.fallback_addresses 最多 {MAX_FALLBACK_ADDRESSES} 個（目前 {} 個）",
            items.len()
        ));
    }
    for item in items {
        parse_fallback(item, default_port)
            .map_err(|e| format!("server.fallback_addresses 的 {e}"))?;
    }
    Ok(())
}

/// `fallback_addresses` 的反序列化：TOML 的字串陣列，或環境變數的逗號分隔字串。
/// 去除每一項的前後空白並略過空字串
pub fn de_address_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        List(Vec<String>),
        Text(String),
    }
    let items = match Raw::deserialize(d)? {
        Raw::List(v) => v,
        Raw::Text(s) => s.split(',').map(str::to_string).collect(),
    };
    Ok(items
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_items_follow_the_endpoint_rules() {
        let ok = |s: &str| parse_fallback(s, 8443).unwrap().to_string();
        assert_eq!(ok("kairos-b.example.com"), "kairos-b.example.com:8443");
        assert_eq!(ok("Kairos-B.Example.com:9443"), "kairos-b.example.com:9443");
        assert_eq!(ok(" 10.0.20.11:8443 "), "10.0.20.11:8443");
        assert_eq!(ok("10.0.20.11"), "10.0.20.11:8443");
        assert_eq!(ok("[2001:DB8::10]"), "[2001:db8::10]:8443");
        assert_eq!(ok("[2001:db8::10]:9000"), "[2001:db8::10]:9000");
        assert_eq!(parse_fallback("[::1]", 1).unwrap().host, "::1");
        for bad in [
            "2001:db8::10",
            "host:0",
            "host:65536",
            "host:abc",
            "bad_host",
            "010.0.0.1",
            "[zz::1]",
            "a..b",
            ":8443",
        ] {
            assert!(parse_fallback(bad, 8443).is_err(), "{bad}");
        }
        assert!(
            parse_fallback("2001:db8::10", 8443)
                .unwrap_err()
                .contains("方括號")
        );
    }

    #[test]
    fn candidate_list_starts_with_the_primary_and_drops_duplicates() {
        let list = candidates(
            "Kairos-A.example.com",
            8443,
            &[
                "kairos-a.example.com".into(),
                "kairos-b.example.com".into(),
                " ".into(),
                "kairos-b.example.com:8443".into(),
                "kairos-b.example.com:9443".into(),
                "not valid!".into(),
            ],
        );
        let shown: Vec<String> = list.iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            [
                "Kairos-A.example.com:8443",
                "kairos-b.example.com:8443",
                "kairos-b.example.com:9443"
            ]
        );
        // 只有主位址
        assert_eq!(candidates("127.0.0.1", 8080, &[]).len(), 1);
        // 主位址是沒有方括號的 IPv6（既有設定的寫法）
        assert_eq!(candidates("::1", 8080, &[])[0].to_string(), "[::1]:8080");
        assert_eq!(candidates("[::1]", 8080, &[])[0].to_string(), "[::1]:8080");
    }

    #[test]
    fn validation_limits_the_count_and_reports_the_item() {
        assert!(validate_fallback(&[], 1).is_ok());
        let many: Vec<String> = (0..17).map(|i| format!("h{i}.example.com")).collect();
        assert!(
            validate_fallback(&many, 1)
                .unwrap_err()
                .contains("最多 16 個")
        );
        assert!(validate_fallback(&many[..16], 1).is_ok());
        let e = validate_fallback(&["ok.example.com".into(), "fe80::1".into()], 1).unwrap_err();
        assert!(
            e.contains("server.fallback_addresses") && e.contains("fe80::1"),
            "{e}"
        );
    }

    #[test]
    fn env_text_is_split_on_commas() {
        #[derive(Deserialize)]
        struct T {
            #[serde(deserialize_with = "de_address_list")]
            v: Vec<String>,
        }
        let t: T = serde_json::from_str(r#"{"v":" a.example.com , b:9443,, "}"#).unwrap();
        assert_eq!(t.v, ["a.example.com", "b:9443"]);
        let t: T = serde_json::from_str(r#"{"v":""}"#).unwrap();
        assert!(t.v.is_empty());
        let t: T = serde_json::from_str(r#"{"v":["x", " ", " y "]}"#).unwrap();
        assert_eq!(t.v, ["x", "y"]);
    }
}
