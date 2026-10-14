//! 端點 `host:port` 的正規化（安裝字串的 `s`／`e`、簽章內容的最後一行）。
//!
//! 規則（ENROLL.md「端點的正規寫法」）：
//! - 一定要有連接埠（1～65535，十進位，去掉前導 0）。
//! - 主機為 DNS 名稱時轉小寫、去掉結尾的 `.`；每段 1～63 個英文字母、數字或 `-`（不得以 `-` 開頭或
//!   結尾），全長最多 253 個字元，最後一段不得全是數字。
//! - IPv4 為點分十進位（不接受前導 0）；IPv6 必須以 `[]` 包住，輸出為 RFC 5952 的壓縮小寫寫法。

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

/// 端點格式錯誤
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointError(pub &'static str);

impl fmt::Display for EndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "endpoint must be host:port ({})", self.0)
    }
}

impl std::error::Error for EndpointError {}

fn parse_port(s: &str) -> Result<u16, EndpointError> {
    if s.is_empty() || s.len() > 5 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(EndpointError("port must be 1..=65535"));
    }
    match s.parse::<u32>() {
        Ok(p @ 1..=65535) => Ok(p as u16),
        _ => Err(EndpointError("port must be 1..=65535")),
    }
}

fn normalize_dns(host: &str) -> Result<String, EndpointError> {
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    if host.is_empty() || host.len() > 253 {
        return Err(EndpointError("host name must be 1..=253 characters"));
    }
    let labels: Vec<&str> = host.split('.').collect();
    for label in &labels {
        let ok = (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if !ok {
            return Err(EndpointError("invalid host name"));
        }
    }
    if labels
        .last()
        .is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(EndpointError("invalid IPv4 address or host name"));
    }
    Ok(host)
}

/// 把 `host:port` 轉成正規寫法；不合法時回傳錯誤
pub fn normalize_endpoint(input: &str) -> Result<String, EndpointError> {
    if let Some(rest) = input.strip_prefix('[') {
        let (addr, port) = rest
            .split_once("]:")
            .ok_or(EndpointError("IPv6 address must be written as [addr]:port"))?;
        let ip: Ipv6Addr = addr
            .parse()
            .map_err(|_| EndpointError("invalid IPv6 address"))?;
        return Ok(format!("[{ip}]:{}", parse_port(port)?));
    }
    let (host, port) = input
        .rsplit_once(':')
        .ok_or(EndpointError("port is required"))?;
    if host.contains(':') {
        return Err(EndpointError("IPv6 address must be written as [addr]:port"));
    }
    let port = parse_port(port)?;
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(format!("{ip}:{port}"));
    }
    Ok(format!("{}:{port}", normalize_dns(host)?))
}

/// 端點的主機部分（IPv6 不含 `[]`），供 TLS 的伺服器名稱使用
pub fn endpoint_host(normalized: &str) -> &str {
    match normalized.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or(rest, |(h, _)| h),
        None => normalized.rsplit_once(':').map_or(normalized, |(h, _)| h),
    }
}
