//! 外部 URL 的版本庫（`artifact.source = "url"`；REMOTE.md 第 4.1.2 節）兩端共用的 URL 規則：
//! Kairos 匯入時以同一套規則檢查 `base_url`，Pulse 以它解析提議的 URL 並比對本機白名單
//! （`upgrade.url_allowlist`）。規則刻意比一般的 URL 嚴格，讓「前綴比對」不會被繞過。

use super::MAX_ARTIFACT_URL_BYTES;

/// 解析後的 `https://` URL
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpsUrl {
    /// 小寫的主機名稱或 IP（IPv6 不含方括號）
    pub host: String,
    pub port: u16,
    /// 以 `/` 開頭的路徑
    pub path: String,
}

impl HttpsUrl {
    /// `Host` 標頭與正規化比較用的 authority（預設連接埠不寫）
    pub fn authority(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == 443 {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }

    /// 正規化後的全文（比對白名單用）
    pub fn normalized(&self) -> String {
        format!("https://{}{}", self.authority(), self.path)
    }
}

/// 解析並檢查 URL：只接受 `https://<主機>[:<埠>]/<路徑>`；不接受帳號密碼（`@`）、查詢字串、片段、
/// 反斜線、空白與控制字元、百分比編碼（避免以 `%2e%2e`、`%2f` 繞過前綴比對）、`.`／`..` 路徑段與空的路徑段
/// （結尾的 `/` 可以）
pub fn parse_https_url(url: &str) -> Result<HttpsUrl, String> {
    if url.len() > MAX_ARTIFACT_URL_BYTES {
        return Err(format!("URL 超過 {MAX_ARTIFACT_URL_BYTES} bytes"));
    }
    let rest = url
        .get(..8)
        .filter(|p| p.eq_ignore_ascii_case("https://"))
        .map(|_| &url[8..])
        .ok_or("URL 必須以 https:// 開頭")?;
    if url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '\\' | '?' | '#' | '%' | '@'))
    {
        return Err("URL 不可含空白、控制字元、\\、?、#、%、@".to_string());
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (h, after) = v6.split_once(']').ok_or("IPv6 位址缺少 ]")?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse::<u16>().map_err(|_| "連接埠不合法")?,
            None if after.is_empty() => 443,
            None => return Err("主機名稱不合法".to_string()),
        };
        (h, port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().map_err(|_| "連接埠不合法")?),
            None => (authority, 443),
        }
    };
    if host.is_empty()
        || port == 0
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
    {
        return Err("主機名稱不合法".to_string());
    }
    let segments: Vec<&str> = path[1..].split('/').collect();
    let last = segments.len() - 1;
    if segments
        .iter()
        .enumerate()
        .any(|(i, seg)| *seg == "." || *seg == ".." || (seg.is_empty() && i != last))
    {
        return Err("路徑不可含 .、.. 或空的路徑段".to_string());
    }
    Ok(HttpsUrl {
        host: host.to_ascii_lowercase(),
        port,
        path: path.to_string(),
    })
}

/// URL 是否符合白名單的任一個前綴。白名單的每一項視為「目錄」：比對前補上結尾的 `/`，所以
/// `https://repo.example.gov.tw` 不會放行 `https://repo.example.gov.tw.evil.example/`，
/// `https://repo/kairos-pulse` 也不會放行 `https://repo/kairos-pulse-evil/`。主機名稱不分大小寫，路徑區分。
/// 白名單中不合法的項目（不是 https、含查詢字串…）一律不比對
pub fn url_allowed(url: &str, allowlist: &[String]) -> bool {
    let Ok(u) = parse_https_url(url) else {
        return false;
    };
    let full = u.normalized();
    allowlist.iter().any(|p| {
        let Ok(p) = parse_https_url(p.trim()) else {
            return false;
        };
        let mut prefix = p.normalized();
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        full.starts_with(&prefix)
    })
}

/// URL 的最後一個路徑段必須等於檔名（提議的 `url` 為 `base_url` ＋檔名）
pub fn url_names_file(url: &str, filename: &str) -> bool {
    parse_https_url(url).is_ok_and(|u| u.path.rsplit('/').next() == Some(filename))
}

/// 外部版本庫的 `base_url`（Kairos 匯入時）：合法的 `https://` URL，正規化為以 `/` 結尾；
/// 加上最長的發佈檔名後仍不超過 URL 的上限
pub fn normalize_base_url(base_url: &str) -> Result<String, String> {
    let u = parse_https_url(base_url.trim())?;
    let mut s = u.normalized();
    if !s.ends_with('/') {
        s.push('/');
    }
    if s.len() + 256 > MAX_ARTIFACT_URL_BYTES {
        return Err(format!(
            "base_url 太長（加上檔名後不可超過 {MAX_ARTIFACT_URL_BYTES} bytes）"
        ));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn urls_are_parsed_strictly() {
        let u =
            parse_https_url("https://Repo.Example.gov.tw/kairos-pulse/0.10.0/pulse.deb").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("repo.example.gov.tw", 443, "/kairos-pulse/0.10.0/pulse.deb")
        );
        assert_eq!(u.authority(), "repo.example.gov.tw");
        let u = parse_https_url("https://[::1]:8443/x").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 8443));
        assert_eq!(u.authority(), "[::1]:8443");
        assert_eq!(parse_https_url("https://h").unwrap().path, "/");
        assert!(parse_https_url("https://h/dir/").is_ok());
        for bad in [
            "http://repo/x",
            "ftp://repo/x",
            "https://",
            "https:///x",
            "https://user@repo/x",
            "https://user:pw@repo/x",
            "https://repo/x?y=1",
            "https://repo/x#frag",
            "https://repo/a/../x",
            "https://repo/a/./x",
            "https://repo/a//x",
            "https://repo/%2e%2e/x",
            "https://repo/a b",
            "https://repo\\evil/x",
            "https://repo:0/x",
            "https://repo:99999/x",
            "https://re_po/x",
            "https://repo/x\n",
        ] {
            assert!(parse_https_url(bad).is_err(), "{bad}");
        }
        let long = format!("https://repo/{}", "a".repeat(MAX_ARTIFACT_URL_BYTES));
        assert!(parse_https_url(&long).is_err());
    }

    #[test]
    fn allowlist_is_a_directory_prefix_match() {
        let allow = list(&["https://repo.example.gov.tw/kairos-pulse"]);
        assert!(url_allowed(
            "https://repo.example.gov.tw/kairos-pulse/0.10.0/pulse-0.10.0-abc-linux-x86_64.deb",
            &allow
        ));
        // 主機名稱不分大小寫、預設連接埠
        assert!(url_allowed(
            "https://REPO.example.gov.tw:443/kairos-pulse/x",
            &allow
        ));
        for bad in [
            // 前綴的字串延伸（不同的主機或目錄）
            "https://repo.example.gov.tw.evil.example/kairos-pulse/x",
            "https://repo.example.gov.tw/kairos-pulse-evil/x",
            "https://repo.example.gov.tw/kairos-pulse",
            "https://repo.example.gov.tw/other/x",
            "https://repo.example.gov.tw:8443/kairos-pulse/x",
            "https://repo.example.gov.tw/Kairos-Pulse/x",
            "http://repo.example.gov.tw/kairos-pulse/x",
            "https://evil@repo.example.gov.tw/kairos-pulse/x",
            "https://repo.example.gov.tw/kairos-pulse/../secret",
        ] {
            assert!(!url_allowed(bad, &allow), "{bad}");
        }
        // 只有主機的白名單：補上結尾的 /
        let host_only = list(&["https://repo.example.gov.tw"]);
        assert!(url_allowed("https://repo.example.gov.tw/a/b", &host_only));
        assert!(!url_allowed(
            "https://repo.example.gov.tw.evil.example/a",
            &host_only
        ));
        // 空的白名單（預設）：一律不允許；不合法的白名單項目不比對
        assert!(!url_allowed("https://repo.example.gov.tw/a", &[]));
        assert!(!url_allowed(
            "https://repo/a",
            &list(&["http://repo/", "https://repo/?x", ""])
        ));
        assert!(url_names_file("https://repo/dir/pulse.deb", "pulse.deb"));
        assert!(!url_names_file("https://repo/dir/pulse.deb", "other.deb"));
        assert!(!url_names_file("https://repo/dir/pulse.deb/", "pulse.deb"));
    }

    #[test]
    fn base_urls_are_normalized_to_a_directory() {
        assert_eq!(
            normalize_base_url(" https://Repo.example.gov.tw:443/kairos-pulse/0.10.0 ").unwrap(),
            "https://repo.example.gov.tw/kairos-pulse/0.10.0/"
        );
        assert_eq!(
            normalize_base_url("https://repo.example/").unwrap(),
            "https://repo.example/"
        );
        for bad in [
            "http://repo.example/x/",
            "https://repo.example/x?token=1",
            "https://user@repo.example/",
            "",
        ] {
            assert!(normalize_base_url(bad).is_err(), "{bad}");
        }
        let long = format!("https://repo.example/{}/", "a".repeat(1900));
        assert!(normalize_base_url(&long).is_err());
    }
}
