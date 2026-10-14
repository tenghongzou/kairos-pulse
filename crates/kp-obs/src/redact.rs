//! 敏感資料遮蔽（`design-system/cli-style.md` §10、`pulse/cli-spec.md` §8.3）。
//!
//! 一律遮蔽（不能關閉）：
//!
//! | 類別 | 例 | 遮蔽後 |
//! |---|---|---|
//! | PEM 私鑰區塊 | `-----BEGIN PRIVATE KEY-----…` | `[已遮蔽：私鑰]` |
//! | 主控台 API token | `kat_3f9c2a71b4e05d18_<祕密>` | `kat_3f9c2a71b4e05d18_[已遮蔽]` |
//! | 註冊 token | `KRT1-7G4Q-M2XD-…` | `KRT1-7G4Q…`（只留前綴後的前 4 碼） |
//! | 安裝字串 | `pulse-enroll:v1:…` | `pulse-enroll:v1:[已遮蔽]` |
//! | Bearer | `Authorization: Bearer xxx` | `Bearer [已遮蔽]` |
//! | 設定與 JSON 中名稱含 token、secret、password、passphrase 的值 | `smtp_password = "x"`、`"poll_secret": "…"` | `"[已遮蔽]"` |
//!
//! 名稱以 `_path`、`_file`、`_dir`、`_prefix`、`_hint`、`_id` 結尾的鍵（例如 `metrics_token_file`、
//! `token_prefix`）只是路徑或提示，不遮蔽。可選的 [`Pseudonyms`] 另外把 IP 位址與指定的主機名稱換成
//! 一致的代號（`ip-1`、`host-1`），對照表不放進診斷包。

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// 遮蔽後的標記
pub const MASK: &str = "[已遮蔽]";
pub const MASK_KEY: &str = "[已遮蔽：私鑰]";

/// 各類別遮蔽的處數（`REDACTION.txt`）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub private_key: u64,
    pub api_token: u64,
    pub enroll_token: u64,
    pub install_string: u64,
    pub bearer: u64,
    pub secret_value: u64,
    pub pseudonymized: u64,
}

impl Counts {
    pub fn add(&mut self, o: &Counts) {
        self.private_key += o.private_key;
        self.api_token += o.api_token;
        self.enroll_token += o.enroll_token;
        self.install_string += o.install_string;
        self.bearer += o.bearer;
        self.secret_value += o.secret_value;
        self.pseudonymized += o.pseudonymized;
    }

    /// (類別, 處數) 的清單
    pub fn items(&self) -> [(&'static str, u64); 7] {
        [
            ("private_key", self.private_key),
            ("api_token", self.api_token),
            ("enroll_token", self.enroll_token),
            ("install_string", self.install_string),
            ("bearer", self.bearer),
            ("secret_value", self.secret_value),
            ("pseudonymized", self.pseudonymized),
        ]
    }
}

/// 鍵名是否表示秘密值（最後一段含 token、secret、password 等，且不是路徑或提示）
pub fn secret_key(key: &str) -> bool {
    let k = key.trim().trim_matches('"').to_ascii_lowercase();
    let k = k.rsplit('.').next().unwrap_or(&k).to_string();
    let words = [
        "token",
        "secret",
        "password",
        "passwd",
        "passphrase",
        "bearer",
    ];
    if !words.iter().any(|w| k.contains(w)) {
        return false;
    }
    let harmless = [
        "_path", "_file", "_dir", "_prefix", "_hint", "_id", "_days", "_hours", "_secs", "_ms",
        "_count", "_at", "_name", "_enabled",
    ];
    !harmless.iter().any(|s| k.ends_with(s))
}

/// 遮蔽一段文字
pub fn redact(text: &str, counts: &mut Counts) -> String {
    let s = redact_pem(text, counts);
    let s = redact_lines(&s, counts);
    let s = redact_api_tokens(&s, counts);
    let s = redact_enroll_tokens(&s, counts);
    let s = redact_install_strings(&s, counts);
    redact_bearer(&s, counts)
}

fn redact_pem(text: &str, counts: &mut Counts) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("-----BEGIN ") {
        let after = &rest[start..];
        let header_end = after
            .find("-----\n")
            .or_else(|| after[11..].find("-----").map(|i| i + 11));
        let header = header_end.map_or(after, |e| &after[..e]);
        if !header.contains("PRIVATE KEY") {
            out.push_str(&rest[..start + 11]);
            rest = &rest[start + 11..];
            continue;
        }
        out.push_str(&rest[..start]);
        out.push_str(MASK_KEY);
        counts.private_key += 1;
        match after.find("-----END ") {
            Some(end) => {
                let tail = &after[end + 9..];
                let close = tail.find("-----").map_or(tail.len(), |i| i + 5);
                rest = &tail[close..];
            }
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// `key = value`、`KEY=value`、`"key": "value"` 形式中，鍵名表示秘密的值
fn redact_lines(text: &str, counts: &mut Counts) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&redact_line(line, counts));
    }
    out
}

fn redact_line(line: &str, counts: &mut Counts) -> String {
    // JSON："key": "value"（同一行可能有多組）
    let mut out = String::new();
    let mut rest = line;
    let mut changed = false;
    while let Some(q) = rest.find('"') {
        let after = &rest[q + 1..];
        let Some(kend) = after.find('"') else { break };
        let key = &after[..kend];
        let tail = &after[kend + 1..];
        let t = tail.trim_start();
        if let Some(v) = t.strip_prefix(':') {
            let v = v.trim_start();
            if secret_key(key) && v.starts_with('"') {
                // 找值的結束引號（跳過 \"）
                let bytes = v.as_bytes();
                let mut j = 1;
                while j < bytes.len() && bytes[j] != b'"' {
                    j += if bytes[j] == b'\\' { 2 } else { 1 };
                }
                if j < bytes.len() && j > 1 {
                    let consumed = rest.len() - v.len();
                    out.push_str(&rest[..consumed]);
                    out.push('"');
                    out.push_str(MASK);
                    out.push('"');
                    counts.secret_value += 1;
                    changed = true;
                    rest = &v[j + 1..];
                    continue;
                }
            }
        }
        out.push_str(&rest[..q + 1 + kend + 1]);
        rest = tail;
    }
    if changed {
        out.push_str(rest);
        return out;
    }
    // TOML／環境變數：key = value、KEY=value（行首，可縮排；`export` 前綴）
    let trimmed = line.trim_start();
    let body = trimmed.strip_prefix("export ").unwrap_or(trimmed);
    if let Some((k, v)) = body.split_once('=') {
        let key = k.trim();
        if !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '"'))
            && secret_key(key)
            && !v.trim().is_empty()
            && v.trim() != "\"\""
        {
            // 行首的空白與 `export ` 沒有 `=`，第一個 `=` 就是鍵與值的分隔
            let eq = line.find('=').unwrap_or(line.len() - 1);
            let sep = if line[eq + 1..].starts_with(' ') {
                " "
            } else {
                ""
            };
            counts.secret_value += 1;
            return format!("{}{sep}\"{MASK}\"", &line[..=eq]);
        }
    }
    line.to_string()
}

fn is_b64url(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

fn redact_api_tokens(text: &str, counts: &mut Counts) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(p) = rest.find("kat_") {
        out.push_str(&rest[..p]);
        let after = &rest[p + 4..];
        let id_len = after.chars().take_while(|c| c.is_ascii_hexdigit()).count();
        let tail = &after[id_len..];
        if id_len >= 8 && tail.starts_with('_') {
            let secret_len = tail[1..].chars().take_while(|c| is_b64url(*c)).count();
            if secret_len > 0 && !tail[1..].starts_with(MASK) {
                out.push_str("kat_");
                out.push_str(&after[..id_len]);
                out.push('_');
                out.push_str(MASK);
                counts.api_token += 1;
                rest = &tail[1 + secret_len..];
                continue;
            }
        }
        out.push_str("kat_");
        rest = after;
    }
    out.push_str(rest);
    out
}

fn redact_enroll_tokens(text: &str, counts: &mut Counts) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(p) = rest.find("KRT1") {
        out.push_str(&rest[..p]);
        let after = &rest[p + 4..];
        // 收集 token 字元（英數，允許 - 分隔）
        let mut symbols = String::new();
        let mut used = 0;
        for c in after.chars() {
            if c.is_ascii_alphanumeric() {
                symbols.push(c);
            } else if c != '-' {
                break;
            }
            used += c.len_utf8();
        }
        if symbols.len() > 4 {
            out.push_str("KRT1-");
            out.push_str(&symbols[..4]);
            out.push('…');
            counts.enroll_token += 1;
            rest = &after[used..];
        } else {
            out.push_str("KRT1");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn redact_install_strings(text: &str, counts: &mut Counts) -> String {
    const P: &str = "pulse-enroll:v1:";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(p) = rest.find(P) {
        out.push_str(&rest[..p + P.len()]);
        let after = &rest[p + P.len()..];
        let len = after
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
            .map(char::len_utf8)
            .sum::<usize>();
        if len > 0 && !after.starts_with(MASK) {
            out.push_str(MASK);
            counts.install_string += 1;
        } else {
            out.push_str(&after[..len]);
        }
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

fn redact_bearer(text: &str, counts: &mut Counts) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(p) = rest.find("Bearer ").or_else(|| rest.find("bearer ")) {
        out.push_str(&rest[..p + 7]);
        let after = &rest[p + 7..];
        let len = after
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
            .map(char::len_utf8)
            .sum::<usize>();
        if len > 0 && !after.starts_with(MASK) {
            out.push_str(MASK);
            counts.bearer += 1;
        } else {
            out.push_str(&after[..len]);
        }
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

/// IP 位址與主機名稱的一致化代號（`--redact-hosts`／`--redact strict`）
#[derive(Debug, Default)]
pub struct Pseudonyms {
    hosts: Vec<String>,
    map: BTreeMap<String, String>,
    next_ip: usize,
    next_host: usize,
}

impl Pseudonyms {
    /// `hosts`：要換掉的主機名稱（設定中的位址、本機名稱等；不分大小寫）
    pub fn new(hosts: impl IntoIterator<Item = String>) -> Self {
        let mut hosts: Vec<String> = hosts
            .into_iter()
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| h.len() >= 2 && h.parse::<IpAddr>().is_err() && h != "localhost")
            .collect();
        // 長的先換（避免 a.example.com 先被 example.com 換掉一半）
        hosts.sort_by_key(|h| std::cmp::Reverse(h.len()));
        hosts.dedup();
        Self {
            hosts,
            ..Self::default()
        }
    }

    /// 代號對照表（原值 → 代號；只給使用者另存，不放進診斷包）
    pub fn mapping(&self) -> &BTreeMap<String, String> {
        &self.map
    }

    fn ip_alias(&mut self, ip: IpAddr) -> String {
        let key = ip.to_string();
        if let Some(a) = self.map.get(&key) {
            return a.clone();
        }
        self.next_ip += 1;
        let private = match ip {
            IpAddr::V4(v) => v.is_private() || v.is_link_local(),
            IpAddr::V6(v) => (v.segments()[0] & 0xfe00) == 0xfc00,
        };
        let a = format!(
            "ip-{}{}",
            self.next_ip,
            if private { "-private" } else { "" }
        );
        self.map.insert(key, a.clone());
        a
    }

    fn host_alias(&mut self, host: &str) -> String {
        if let Some(a) = self.map.get(host) {
            return a.clone();
        }
        self.next_host += 1;
        let a = format!("host-{}", self.next_host);
        self.map.insert(host.to_string(), a.clone());
        a
    }

    /// 換掉 IP 與主機名稱（loopback 與未指定位址保留）
    pub fn apply(&mut self, text: &str, counts: &mut Counts) -> String {
        let mut s = text.to_string();
        for h in self.hosts.clone() {
            let mut out = String::with_capacity(s.len());
            let lower = s.to_ascii_lowercase();
            let mut i = 0;
            while let Some(p) = lower[i..].find(&h) {
                let start = i + p;
                let end = start + h.len();
                let boundary = |c: Option<char>| {
                    c.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '.'))
                };
                let before = s[..start].chars().next_back();
                let after = s[end..].chars().next();
                if boundary(before) && boundary(after) {
                    out.push_str(&s[i..start]);
                    out.push_str(&self.host_alias(&h));
                    counts.pseudonymized += 1;
                } else {
                    out.push_str(&s[i..end]);
                }
                i = end;
            }
            out.push_str(&s[i..]);
            s = out;
        }
        self.apply_ips(&s, counts)
    }

    fn apply_ips(&mut self, text: &str, counts: &mut Counts) -> String {
        let chars: Vec<char> = text.chars().collect();
        let is_tok = |c: char| c.is_ascii_hexdigit() || matches!(c, '.' | ':' | '[' | ']');
        let word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < chars.len() {
            if !is_tok(chars[i]) {
                out.push(chars[i]);
                i += 1;
                continue;
            }
            let start = i;
            while i < chars.len() && is_tok(chars[i]) {
                i += 1;
            }
            let token: String = chars[start..i].iter().collect();
            let before_ok = start == 0 || !word(chars[start - 1]);
            let after_ok = i == chars.len() || !word(chars[i]);
            if before_ok && after_ok {
                out.push_str(&self.replace_token(&token, counts));
            } else {
                out.push_str(&token);
            }
        }
        out
    }

    /// 一段由十六進位字元、`.`、`:`、`[`、`]` 組成的文字：IPv4、IPv4:埠、IPv6、[IPv6]、[IPv6]:埠
    fn replace_token(&mut self, token: &str, counts: &mut Counts) -> String {
        let t = token.trim_end_matches(['.', ':']);
        let tail = &token[t.len()..];
        let port_ok = |p: &str| {
            p.is_empty()
                || p.strip_prefix(':')
                    .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
        };
        let keep = |ip: IpAddr| ip.is_loopback() || ip.is_unspecified();
        if let Some(r) = t.strip_prefix('[') {
            if let Some((inner, rest)) = r.split_once(']')
                && let Ok(v6) = inner.parse::<Ipv6Addr>()
                && port_ok(rest)
                && !keep(IpAddr::V6(v6))
            {
                counts.pseudonymized += 1;
                return format!("[{}]{rest}{tail}", self.ip_alias(IpAddr::V6(v6)));
            }
            return token.to_string();
        }
        let (host, port) = match t.rsplit_once(':') {
            Some((h, _)) if h.parse::<Ipv4Addr>().is_ok() => (h, &t[h.len()..]),
            _ => (t, ""),
        };
        if port_ok(port)
            && let Ok(v4) = host.parse::<Ipv4Addr>()
        {
            if keep(IpAddr::V4(v4)) {
                return token.to_string();
            }
            counts.pseudonymized += 1;
            return format!("{}{port}{tail}", self.ip_alias(IpAddr::V4(v4)));
        }
        if t.matches(':').count() >= 2
            && let Ok(v6) = t.parse::<Ipv6Addr>()
            && !keep(IpAddr::V6(v6))
        {
            counts.pseudonymized += 1;
            return format!("{}{tail}", self.ip_alias(IpAddr::V6(v6)));
        }
        token.to_string()
    }
}

/// 是否還有看起來像秘密的內容（測試與 diag 產生後的自我檢查）：PEM 私鑰、完整的 API token、
/// 完整的註冊 token、未遮蔽的安裝字串與 Bearer
pub fn find_leak(text: &str) -> Option<String> {
    if text.contains("PRIVATE KEY-----") {
        return Some("PEM 私鑰".into());
    }
    let mut c = Counts::default();
    if redact_api_tokens(text, &mut c) != text {
        return Some("API token".into());
    }
    if redact_enroll_tokens(text, &mut c) != text {
        return Some("註冊 token".into());
    }
    if redact_install_strings(text, &mut c) != text {
        return Some("安裝字串".into());
    }
    if redact_bearer(text, &mut c) != text {
        return Some("Bearer".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg\n-----END PRIVATE KEY-----\n";

    #[test]
    fn private_keys_are_removed() {
        let mut c = Counts::default();
        let text = format!(
            "前\n{KEY}中\n-----BEGIN EC PRIVATE KEY-----\nabc\n-----END EC PRIVATE KEY-----\n-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n後"
        );
        let r = redact(&text, &mut c);
        assert_eq!(c.private_key, 2);
        assert!(!r.contains("MIGHAgEA") && !r.contains("abc"));
        assert!(r.contains("BEGIN CERTIFICATE"), "憑證不是秘密：{r}");
        assert!(
            r.starts_with("前\n[已遮蔽：私鑰]\n中\n[已遮蔽：私鑰]\n"),
            "{r}"
        );
        assert!(find_leak(&r).is_none());
        // 沒有結尾的區塊：遮蔽到結尾
        let mut c = Counts::default();
        let r = redact("x -----BEGIN RSA PRIVATE KEY-----\nAAAA", &mut c);
        assert_eq!(r, "x [已遮蔽：私鑰]");
    }

    #[test]
    fn tokens_and_secrets_are_masked() {
        let mut c = Counts::default();
        let text = "\
[smtp]
password = \"hunter2\"
metrics_token_file = \"/opt/kairos/metrics.token\"
KAIROS__CONSOLE__TOKEN=abcdef
token_prefix = \"KRT1-7G4Q\"
api: kat_3f9c2a71b4e05d18_Zm9vYmFyYmF6cXV4cXV1eA-_x end
enroll: KRT1-7G4Q-M2XD-9KQP-A1B2 end
install: pulse-enroll:v1:eyJzIjoieCJ9.sig end
curl -H \"Authorization: Bearer kat_abc\" http://x
{\"poll_secret\": \"0123456789abcdef\", \"token_hint\": \"KRT1-7G4Q…\", \"name\": \"ok\"}
";
        let r = redact(text, &mut c);
        assert!(!r.contains("hunter2"), "{r}");
        assert!(r.contains("metrics_token_file = \"/opt/kairos/metrics.token\""));
        assert!(!r.contains("abcdef\n"), "{r}");
        assert!(r.contains("token_prefix = \"KRT1-7G4Q\""), "{r}");
        assert!(r.contains("kat_3f9c2a71b4e05d18_[已遮蔽] end"), "{r}");
        assert!(r.contains("enroll: KRT1-7G4Q… end"), "{r}");
        assert!(r.contains("pulse-enroll:v1:[已遮蔽] end"), "{r}");
        assert!(r.contains("Bearer [已遮蔽]\""), "{r}");
        assert!(!r.contains("0123456789abcdef"), "{r}");
        assert!(r.contains("\"token_hint\": \"KRT1-7G4Q…\""), "{r}");
        assert!(r.contains("\"name\": \"ok\""));
        assert_eq!(c.api_token, 1);
        assert_eq!(c.enroll_token, 1);
        assert_eq!(c.install_string, 1);
        assert_eq!(c.bearer, 1);
        assert_eq!(c.secret_value, 3);
        assert!(find_leak(&r).is_none(), "{r}");
        assert!(find_leak(text).is_some());
        // 冪等
        let mut c2 = Counts::default();
        assert_eq!(redact(&r, &mut c2), r);
    }

    #[test]
    fn secret_key_names() {
        for k in [
            "password",
            "smtp.password",
            "api_token",
            "\"poll_secret\"",
            "BIND_PASSWD",
        ] {
            assert!(secret_key(k), "{k}");
        }
        for k in [
            "metrics_token_file",
            "token_prefix",
            "token_id",
            "key_path",
            "hostname",
            "token_hint",
        ] {
            assert!(!secret_key(k), "{k}");
        }
    }

    #[test]
    fn pseudonyms_are_consistent() {
        let mut p = Pseudonyms::new(["kairos.example.gov.tw".to_string(), "plc-07".to_string()]);
        let mut c = Counts::default();
        let r = p.apply(
            "連線到 kairos.example.gov.tw:8443（10.20.0.15:8443）來自 10.20.0.15、203.0.113.9 與 [2001:db8::1]:8444；127.0.0.1 不換；plc-07 與 plc-070 不同",
            &mut c,
        );
        assert!(r.contains("連線到 host-1:8443"), "{r}");
        assert!(r.contains("（ip-1-private:8443）"), "{r}");
        assert!(r.contains("來自 ip-1-private、ip-2 與 [ip-3]:8444"), "{r}");
        assert!(r.contains("127.0.0.1 不換"), "{r}");
        assert!(r.contains("host-2 與 plc-070"), "{r}");
        assert_eq!(p.mapping()["10.20.0.15"], "ip-1-private");
        assert!(c.pseudonymized >= 6);
    }
}
