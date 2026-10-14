//! CIDR 網段與 `server.per_ip_allowlist`（審計 A7 過渡方案，設計包 D14、D15、D22）。
//!
//! 自行實作，不引入額外依賴。解析規則：
//! - `位址/前綴長度`，或只寫位址（視為單一位址：IPv4 `/32`、IPv6 `/128`）。
//! - 前綴長度只接受十進位數字（不接受 `+24`、空白或十六進位），IPv4 為 0～32、IPv6 為 0～128。
//! - 位址的主機位元必須為 0（例如 `192.0.2.1/24` 拒絕並建議 `192.0.2.0/24`），避免把單一位址誤寫成整個網段。
//! - IPv4-mapped IPv6 網段（`::ffff:a.b.c.d/96..=128`）轉成對應的 IPv4 網段；比對時連線位址也先把
//!   IPv4-mapped IPv6 視為 IPv4（雙堆疊 socket 上的 IPv4 連線），因此兩種寫法等價。
//! - 不接受 zone ID（`fe80::1%eth0`）、方括號與前後空白以外的多餘字元。

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// 一個 CIDR 網段；`network` 的主機位元一定為 0
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

/// CIDR 解析失敗的原因
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CidrError {
    /// 空字串
    Empty,
    /// 位址部分不是合法的 IPv4／IPv6 位址
    Address(String),
    /// 前綴長度不是數字或超出範圍
    Prefix { prefix: String, max: u8 },
    /// 主機位元不為 0
    HostBits { suggestion: Cidr },
}

impl fmt::Display for CidrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CidrError::Empty => write!(f, "不可為空"),
            CidrError::Address(addr) => write!(
                f,
                "\"{}\" 不是合法的 IPv4 或 IPv6 位址（不接受 zone ID、方括號與前導 0）",
                addr.escape_debug()
            ),
            CidrError::Prefix { prefix, max } => write!(
                f,
                "前綴長度 \"{}\" 必須是 0～{} 的十進位整數",
                prefix.escape_debug(),
                max
            ),
            CidrError::HostBits { suggestion } => {
                write!(f, "主機位元不為 0，是否要寫成 {}？", suggestion)
            }
        }
    }
}

impl std::error::Error for CidrError {}

fn v4_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    }
}

fn v6_mask(prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - u32::from(prefix))
    }
}

/// 把 IPv4-mapped IPv6 位址視為 IPv4（與 [`crate::per_ip::address_key`] 相同）
fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    }
}

impl Cidr {
    /// 以位址與前綴長度建立；主機位元不為 0 時回 [`CidrError::HostBits`]（附建議的網段）。
    /// IPv4-mapped IPv6 且前綴長度 ≥ 96 時轉成 IPv4 網段
    pub fn new(addr: IpAddr, prefix: u8) -> Result<Self, CidrError> {
        let (addr, prefix) = match addr {
            IpAddr::V6(v6) if prefix >= 96 => match v6.to_ipv4_mapped() {
                Some(v4) => (IpAddr::V4(v4), prefix - 96),
                None => (addr, prefix),
            },
            _ => (addr, prefix),
        };
        let max = match addr {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        if prefix > max {
            return Err(CidrError::Prefix {
                prefix: prefix.to_string(),
                max,
            });
        }
        let network = Self::mask(addr, prefix);
        let cidr = Self { network, prefix };
        if network != addr {
            return Err(CidrError::HostBits { suggestion: cidr });
        }
        Ok(cidr)
    }

    fn mask(addr: IpAddr, prefix: u8) -> IpAddr {
        match addr {
            IpAddr::V4(v4) => IpAddr::V4(Ipv4Addr::from(u32::from(v4) & v4_mask(prefix))),
            IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & v6_mask(prefix))),
        }
    }

    pub fn network(&self) -> IpAddr {
        self.network
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// 包含 `ip` 的網段（前綴長度超過位址長度時取位址長度；IPv4-mapped IPv6 視為 IPv4）。
    /// 告警的網段彙總以主機最後已知的位址計算（`alerts-ux.md` §3.3 C）
    pub fn containing(ip: IpAddr, v4_prefix: u8, v6_prefix: u8) -> Self {
        let ip = unmap(ip);
        let prefix = match ip {
            IpAddr::V4(_) => v4_prefix.min(32),
            IpAddr::V6(_) => v6_prefix.min(128),
        };
        Self {
            network: Self::mask(ip, prefix),
            prefix,
        }
    }

    /// `ip` 是否在網段內；IPv4-mapped IPv6 位址視為 IPv4，IPv6 網段也能涵蓋 IPv4-mapped 範圍
    /// （例如 `::/0` 包含所有 IPv4 位址）
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.network, unmap(ip)) {
            (IpAddr::V4(net), IpAddr::V4(a)) => {
                u32::from(a) & v4_mask(self.prefix) == u32::from(net)
            }
            (IpAddr::V6(net), IpAddr::V6(a)) => {
                u128::from(a) & v6_mask(self.prefix) == u128::from(net)
            }
            (IpAddr::V6(net), IpAddr::V4(a)) => {
                u128::from(a.to_ipv6_mapped()) & v6_mask(self.prefix) == u128::from(net)
            }
            (IpAddr::V4(_), IpAddr::V6(_)) => false,
        }
    }

    /// 網段與 IPv6 `/64` 來源（[`crate::per_ip::source_key`] 的結果）是否有交集。
    /// 新連線速率與未啟用 mTLS 時的 report 額度以 /64 為來源，需要以此判斷是否套用允許清單
    pub fn overlaps_source(&self, key: IpAddr) -> bool {
        match (self.network, key) {
            (IpAddr::V6(net), IpAddr::V6(k)) => {
                let mask = v6_mask(self.prefix.min(64));
                u128::from(k) & mask == u128::from(net) & mask
            }
            _ => self.contains(key),
        }
    }
}

impl std::str::FromStr for Cidr {
    type Err = CidrError;

    fn from_str(s: &str) -> Result<Self, CidrError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(CidrError::Empty);
        }
        let (addr_text, prefix_text) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = addr_text
            .parse()
            .map_err(|_| CidrError::Address(addr_text.to_string()))?;
        let max = match addr {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        let prefix = match prefix_text {
            None => max,
            Some(p) => {
                let bad = || CidrError::Prefix {
                    prefix: p.to_string(),
                    max,
                };
                if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(bad());
                }
                match p.parse::<u8>() {
                    Ok(n) if n <= max => n,
                    _ => return Err(bad()),
                }
            }
        };
        Cidr::new(addr, prefix)
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

/// `server.per_ip_allowlist`：清單內來源的每 IP 上限改用
/// `min(server.max_connections_per_allowlisted_ip, server.max_connections)`
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    entries: Vec<Cidr>,
}

impl Allowlist {
    /// 解析設定的清單；空白項目略過（環境變數 `KAIROS__SERVER__PER_IP_ALLOWLIST=""` 表示空清單）。
    /// 錯誤訊息寫出第幾項、原始內容與原因
    pub fn parse<S: AsRef<str>>(items: &[S]) -> Result<Self, String> {
        let mut entries = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let item = item.as_ref();
            if item.trim().is_empty() {
                continue;
            }
            let cidr: Cidr = item.parse().map_err(|e: CidrError| {
                format!(
                    "server.per_ip_allowlist 第 {} 項 \"{}\" 無效：{}",
                    i + 1,
                    item.escape_debug(),
                    e
                )
            })?;
            if !entries.contains(&cidr) {
                entries.push(cidr);
            }
        }
        Ok(Self { entries })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[Cidr] {
        &self.entries
    }

    /// 連線位址是否在清單內
    pub fn contains(&self, ip: IpAddr) -> bool {
        self.entries.iter().any(|c| c.contains(ip))
    }

    /// 來源（IPv6 以 /64 合併後的 key）是否與清單內任一網段有交集
    pub fn matches_source(&self, key: IpAddr) -> bool {
        self.entries.iter().any(|c| c.overlaps_source(key))
    }
}

impl fmt::Display for Allowlist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, c) in self.entries.iter().enumerate() {
            if i > 0 {
                f.write_str("、")?;
            }
            write!(f, "{}", c)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn cidr(s: &str) -> Cidr {
        s.parse().unwrap()
    }

    #[test]
    fn parses_ipv4_and_ipv6_networks() {
        let c = cidr("192.0.2.0/24");
        assert_eq!((c.network(), c.prefix()), (ip("192.0.2.0"), 24));
        assert_eq!(c.to_string(), "192.0.2.0/24");
        assert!(c.contains(ip("192.0.2.0")));
        assert!(c.contains(ip("192.0.2.255")));
        assert!(!c.contains(ip("192.0.3.0")));
        assert!(!c.contains(ip("2001:db8::1")));

        let c = cidr(" 2001:db8:abcd::/48 ");
        assert_eq!(c.to_string(), "2001:db8:abcd::/48");
        assert!(c.contains(ip("2001:db8:abcd:ffff::1")));
        assert!(!c.contains(ip("2001:db8:abce::1")));
        assert!(!c.contains(ip("192.0.2.1")));

        // 只寫位址：單一位址
        assert_eq!(cidr("203.0.113.10").to_string(), "203.0.113.10/32");
        assert_eq!(cidr("2001:db8::7").to_string(), "2001:db8::7/128");
        assert!(cidr("203.0.113.10").contains(ip("203.0.113.10")));
        assert!(!cidr("203.0.113.10").contains(ip("203.0.113.11")));

        // 邊界：/0 與最長前綴
        assert!(cidr("0.0.0.0/0").contains(ip("255.255.255.255")));
        assert!(cidr("::/0").contains(ip("2001:db8::1")));
        assert!(cidr("10.0.0.1/32").contains(ip("10.0.0.1")));
        assert!(cidr("::1/128").contains(ip("::1")));
        // 前綴長度允許前導 0（十進位）
        assert_eq!(cidr("10.0.0.0/08").prefix(), 8);
    }

    #[test]
    fn rejects_malformed_input() {
        for (input, expected) in [
            ("", CidrError::Empty),
            ("   ", CidrError::Empty),
            (
                "10.0.0.0/",
                CidrError::Prefix {
                    prefix: "".into(),
                    max: 32,
                },
            ),
            (
                "10.0.0.0/33",
                CidrError::Prefix {
                    prefix: "33".into(),
                    max: 32,
                },
            ),
            (
                "10.0.0.0/+8",
                CidrError::Prefix {
                    prefix: "+8".into(),
                    max: 32,
                },
            ),
            (
                "10.0.0.0/-1",
                CidrError::Prefix {
                    prefix: "-1".into(),
                    max: 32,
                },
            ),
            (
                "10.0.0.0/8/8",
                CidrError::Prefix {
                    prefix: "8/8".into(),
                    max: 32,
                },
            ),
            (
                "10.0.0.0/ 8",
                CidrError::Prefix {
                    prefix: " 8".into(),
                    max: 32,
                },
            ),
            (
                "10.0.0.0/0x8",
                CidrError::Prefix {
                    prefix: "0x8".into(),
                    max: 32,
                },
            ),
            (
                "10.0.0.0/0008",
                CidrError::Prefix {
                    prefix: "0008".into(),
                    max: 32,
                },
            ),
            (
                "::/129",
                CidrError::Prefix {
                    prefix: "129".into(),
                    max: 128,
                },
            ),
            (
                "::/256",
                CidrError::Prefix {
                    prefix: "256".into(),
                    max: 128,
                },
            ),
            ("10.0.0/8", CidrError::Address("10.0.0".into())),
            ("010.0.0.0/8", CidrError::Address("010.0.0.0".into())),
            ("256.0.0.0/8", CidrError::Address("256.0.0.0".into())),
            ("example.com/24", CidrError::Address("example.com".into())),
            (
                "fe80::1%eth0/128",
                CidrError::Address("fe80::1%eth0".into()),
            ),
            ("[2001:db8::]/32", CidrError::Address("[2001:db8::]".into())),
            ("/24", CidrError::Address("".into())),
            ("10.0.0.0 /8", CidrError::Address("10.0.0.0 ".into())),
        ] {
            assert_eq!(input.parse::<Cidr>(), Err(expected), "{input:?}");
        }
        // 主機位元不為 0：附建議的網段
        let err = "192.0.2.1/24".parse::<Cidr>().unwrap_err();
        assert_eq!(
            err,
            CidrError::HostBits {
                suggestion: cidr("192.0.2.0/24")
            }
        );
        assert_eq!(err.to_string(), "主機位元不為 0，是否要寫成 192.0.2.0/24？");
        assert!("2001:db8::1/64".parse::<Cidr>().is_err());
    }

    #[test]
    fn ipv4_mapped_ipv6_is_equivalent_to_ipv4() {
        // 寫成 IPv4-mapped IPv6 的網段轉成 IPv4
        assert_eq!(cidr("::ffff:192.0.2.0/120"), cidr("192.0.2.0/24"));
        assert_eq!(cidr("::ffff:192.0.2.10"), cidr("192.0.2.10/32"));
        assert_eq!(cidr("::ffff:0:0/96"), cidr("0.0.0.0/0"));
        assert!("::ffff:192.0.2.1/120".parse::<Cidr>().is_err());

        // 雙堆疊 socket 上的 IPv4 連線（::ffff:a.b.c.d）與 IPv4 網段比對
        let mapped = IpAddr::V6(Ipv4Addr::new(192, 0, 2, 7).to_ipv6_mapped());
        assert!(cidr("192.0.2.0/24").contains(mapped));
        assert!(!cidr("198.51.100.0/24").contains(mapped));
        // 涵蓋 IPv4-mapped 範圍的 IPv6 網段也包含 IPv4 位址
        assert!(cidr("::/0").contains(ip("192.0.2.7")));
        assert!(cidr("::/1").contains(ip("192.0.2.7")));
        assert!(!cidr("2001:db8::/32").contains(ip("192.0.2.7")));
        // 前綴不到 96 的 mapped 網段維持 IPv6，仍以 mapped 形式比對
        let wide = cidr("::fffe:0:0/95");
        assert!(matches!(wide.network(), IpAddr::V6(_)));
        assert!(wide.contains(ip("192.0.2.7")));
    }

    #[test]
    fn source_overlap_uses_the_ipv6_64() {
        let key = ip("2001:db8:1:2::");
        // 較寬的網段：包含整個 /64
        assert!(cidr("2001:db8::/32").overlaps_source(key));
        // 較窄的網段（例如單一 NAT 位址）：與該 /64 有交集
        assert!(cidr("2001:db8:1:2::10/128").overlaps_source(key));
        assert!(cidr("2001:db8:1:2:8000::/65").overlaps_source(key));
        assert!(!cidr("2001:db8:1:3::10/128").overlaps_source(key));
        // IPv4 照常比對
        assert!(cidr("192.0.2.0/24").overlaps_source(ip("192.0.2.9")));
        assert!(!cidr("192.0.2.0/24").overlaps_source(key));
    }

    #[test]
    fn allowlist_parses_lists_and_reports_the_bad_entry() {
        let list =
            Allowlist::parse(&["10.0.0.0/8", " ", "", "2001:db8::/32", "10.0.0.0/8"]).unwrap();
        // 空白項目略過、重複的網段只保留一個
        assert_eq!(list.entries().len(), 2);
        assert_eq!(list.to_string(), "10.0.0.0/8、2001:db8::/32");
        assert!(list.contains(ip("10.1.2.3")));
        assert!(list.contains(ip("::ffff:10.1.2.3")));
        assert!(list.contains(ip("2001:db8::5")));
        assert!(!list.contains(ip("192.0.2.1")));
        assert!(list.matches_source(ip("2001:db8:0:1::")));

        assert!(Allowlist::parse::<&str>(&[]).unwrap().is_empty());
        assert!(Allowlist::parse(&[""]).unwrap().is_empty());

        let err = Allowlist::parse(&["10.0.0.0/8", "10.0.0.1/8"]).unwrap_err();
        assert_eq!(
            err,
            "server.per_ip_allowlist 第 2 項 \"10.0.0.1/8\" 無效：主機位元不為 0，是否要寫成 10.0.0.0/8？"
        );
        let err = Allowlist::parse(&["nat-gw"]).unwrap_err();
        assert!(err.contains("第 1 項 \"nat-gw\""), "{err}");
    }
}
