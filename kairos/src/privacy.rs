//! 個資保護（商業審計 S6；規格 `kairos/PRIVACY.md`）。
//!
//! `[privacy]` 決定 Kairos **保存**（資料庫、`agents.json`、JSONL、主控台 API）哪些個資、以什麼形式保存。
//! 轉換在資料**進入** Kairos 的地方一次做完（[`PrivacyPolicy`]）：8443 的連線（hello 的主機名稱與顯示名稱、
//! 來源位址、report 的行程、`inventory` 的網路介面）與 8444 的註冊申請（主機名稱、顯示名稱）。之後的
//! 狀態表、清冊、report、資產、告警、匯出看到的都是轉換後的值，不需要各自處理。
//!
//! | 設定鍵 | 值 | 預設 |
//! |---|---|---|
//! | `privacy.store_peer_ip` | `full`（`ip:port`）、`truncate`（IPv4 /24、IPv6 /48，不含埠）、`none`（不保存） | `full` |
//! | `privacy.hostname` | `keep`、`pseudonymize`（以 `data/privacy.key` 的 HMAC 換成 `host-xxxxxxxxxxxx`；顯示名稱換成 `name-…`） | `keep` |
//! | `privacy.process_command` | `keep`、`redact`（遮蔽命令列中的密碼、token、帶帳密的網址，與診斷包相同的規則）、`drop` | `redact` |
//! | `privacy.process_user` | `keep`、`pseudonymize`（`user-…`）、`drop` | `keep` |
//! | `privacy.inventory_network` | `keep`、`truncate`（介面位址截到 /24、/48，MAC 只留廠商碼）、`drop`（不保存位址與 MAC） | `keep` |
//!
//! 假名化不可逆（HMAC-SHA256，不保存原值）：知道金鑰的人可以對「候選的主機名稱」算出假名再比對
//! （`kairos hosts pseudonym <主機名稱>`），但無法從假名還原。取捨見 PRIVACY.md 第 3 節。
//!
//! 不受這些設定影響（理由見 PRIVACY.md）：Kairos 的日誌（資安紀錄，依 `log.retention_days`）、稽核日誌
//! 的來源 IP（雜湊鏈不可修改，依 `audit.retention_days`）、註冊申請的來源 IP（防濫用的判斷依據，結束後 30 天刪除）。

use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::io::{Error, ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;

/// 假名化的金鑰檔（32 bytes，0600；在資料目錄中，不在資料庫中）
pub const KEY_FILE: &str = "privacy.key";

/// 要求假名化但金鑰檔無法建立或讀取（Kairos 不啟動）
pub const CODE_KEY_UNAVAILABLE: &str = "KRS-E4005";

/// 假名的雜湊長度（base32 字元；60 位元，1 萬台主機的碰撞機率約 4×10⁻¹¹）
const PSEUDONYM_CHARS: usize = 12;

/// 主機名稱是否為假名（`host-` ＋ 12 個 RFC 4648 base32 小寫字元；主機 API 的 `pseudonymized`）。
/// 假名化不留原值，只能依格式判斷：`privacy.hostname` 改回 `keep` 之後，先前假名化的主機仍是假名；
/// 原本就叫這個格式的主機名稱也會被判為假名（隨機名稱剛好符合的機率可忽略）
pub fn is_host_pseudonym(name: &str) -> bool {
    name.strip_prefix("host-").is_some_and(|rest| {
        rest.len() == PSEUDONYM_CHARS
            && rest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
    })
}

/// `privacy.store_peer_ip`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerIpMode {
    /// `ip:port`（0.8 以前的行為）
    #[default]
    Full,
    /// IPv4 /24、IPv6 /48 的網段（例 `10.0.3.0/24`），不含埠
    Truncate,
    /// 不保存（空字串）
    None,
}

/// `privacy.hostname`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostnameMode {
    #[default]
    Keep,
    /// 以金鑰的 HMAC 換成 `host-xxxxxxxxxxxx`（不可逆、同一個名稱一定得到同一個假名）
    Pseudonymize,
}

/// `privacy.process_command`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessCommandMode {
    Keep,
    /// 遮蔽命令列中的秘密（`--password x`、`-p<密碼>`、`token=…`、Bearer、帶帳密的網址）
    #[default]
    Redact,
    /// 不保存命令列
    Drop,
}

/// `privacy.process_user`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessUserMode {
    #[default]
    Keep,
    /// `user-xxxxxxxxxxxx`
    Pseudonymize,
    Drop,
}

/// `privacy.inventory_network`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryNetworkMode {
    #[default]
    Keep,
    /// 介面位址截到 /24、/48（前綴較短的保留原本的前綴），MAC 只留前 3 組（廠商碼）
    Truncate,
    /// 不保存介面位址與 MAC（介面名稱、MTU 照存）
    Drop,
}

/// `[privacy]`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PrivacySettings {
    pub store_peer_ip: PeerIpMode,
    pub hostname: HostnameMode,
    pub process_command: ProcessCommandMode,
    pub process_user: ProcessUserMode,
    pub inventory_network: InventoryNetworkMode,
}

impl PrivacySettings {
    /// 是否需要假名化的金鑰
    pub fn needs_key(&self) -> bool {
        self.hostname == HostnameMode::Pseudonymize
            || self.process_user == ProcessUserMode::Pseudonymize
    }

    /// 啟動時的一行說明（與預設不同的項目）；全部是預設時為 `None`
    pub fn describe(&self) -> Option<String> {
        let mut parts = Vec::new();
        match self.store_peer_ip {
            PeerIpMode::Full => {}
            PeerIpMode::Truncate => parts.push("來源位址只保存網段（/24、/48）".to_string()),
            PeerIpMode::None => parts.push("不保存來源位址".to_string()),
        }
        if self.hostname == HostnameMode::Pseudonymize {
            parts.push("主機名稱與顯示名稱以假名保存".to_string());
        }
        match self.process_command {
            ProcessCommandMode::Redact => {}
            ProcessCommandMode::Keep => parts.push("行程命令列原樣保存（不遮蔽）".to_string()),
            ProcessCommandMode::Drop => parts.push("不保存行程命令列".to_string()),
        }
        match self.process_user {
            ProcessUserMode::Keep => {}
            ProcessUserMode::Pseudonymize => parts.push("行程的使用者以假名保存".to_string()),
            ProcessUserMode::Drop => parts.push("不保存行程的使用者".to_string()),
        }
        match self.inventory_network {
            InventoryNetworkMode::Keep => {}
            InventoryNetworkMode::Truncate => {
                parts.push("資產的介面位址只保存網段、MAC 只保存廠商碼".to_string());
            }
            InventoryNetworkMode::Drop => parts.push("不保存資產的介面位址與 MAC".to_string()),
        }
        (!parts.is_empty()).then(|| parts.join("；"))
    }
}

/// 假名化的金鑰（由 [`KEY_FILE`] 衍生；原始金鑰不留在記憶體中）
pub struct PseudonymKey {
    mac: hmac::Key,
    key_id: String,
}

impl std::fmt::Debug for PseudonymKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PseudonymKey({})", self.key_id)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl PseudonymKey {
    pub fn from_bytes(key: &[u8; 32]) -> Self {
        let base = hmac::Key::new(hmac::HMAC_SHA256, key);
        let derived = hmac::sign(&base, b"kairos-privacy-pseudonym/v1");
        let id = hmac::sign(&base, b"kairos-privacy-key-id/v1");
        Self {
            mac: hmac::Key::new(hmac::HMAC_SHA256, derived.as_ref()),
            key_id: hex(&id.as_ref()[..8]),
        }
    }

    /// 金鑰的識別（不是秘密；換了金鑰之後假名全部不同，以它判斷）
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// `kind`（`host`、`name`、`user`）＋原值的假名，例 `host-k3m7x2q7v4ab`。比對不分大小寫
    pub fn pseudonym(&self, kind: &str, value: &str) -> String {
        let mut ctx = hmac::Context::with_key(&self.mac);
        ctx.update(kind.as_bytes());
        ctx.update(b"\0");
        ctx.update(value.trim().to_lowercase().as_bytes());
        let tag = ctx.sign();
        format!("{kind}-{}", base32_lower(tag.as_ref(), PSEUDONYM_CHARS))
    }

    /// 讀取資料目錄的金鑰檔；不存在時為 `Ok(None)`（不建立）
    pub fn load(data_dir: &Path) -> Result<Option<Self>, Error> {
        let path = data_dir.join(KEY_FILE);
        let mut buf = zeroize::Zeroizing::new(Vec::new());
        match std::fs::File::open(&path) {
            Ok(mut f) => {
                f.read_to_end(&mut buf)?;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(Error::new(
                    e.kind(),
                    format!("無法讀取 {}: {e}", path.display()),
                ));
            }
        }
        let key: zeroize::Zeroizing<[u8; 32]> =
            zeroize::Zeroizing::new(buf.as_slice().try_into().map_err(|_| {
                Error::new(
                    ErrorKind::InvalidData,
                    format!("{} 的長度不是 32 bytes（檔案可能已損毀）", path.display()),
                )
            })?);
        Ok(Some(Self::from_bytes(&key)))
    }

    /// 讀取金鑰檔；不存在時建立（0600，只寫一次，不覆寫）。只由 Kairos 服務在啟動時呼叫
    pub fn load_or_create(data_dir: &Path) -> Result<Self, Error> {
        if let Some(k) = Self::load(data_dir)? {
            crate::users::secrets::restrict_0600(&data_dir.join(KEY_FILE));
            return Ok(k);
        }
        // 叢集模式下不自動建立（kairos/HA.md 第 11.1 節）
        crate::cluster::keys::check_autocreate(&data_dir.join(KEY_FILE))?;
        let mut key = zeroize::Zeroizing::new([0u8; 32]);
        SystemRandom::new()
            .fill(&mut key[..])
            .map_err(|_| Error::other("無法取得隨機數（作業系統的 CSPRNG）"))?;
        let path = data_dir.join(KEY_FILE);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&path) {
            Ok(mut f) => {
                f.write_all(&key[..])?;
                f.sync_all()?;
                Ok(Self::from_bytes(&key))
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => Self::load(data_dir)?
                .ok_or_else(|| Error::other(format!("{} 剛建立卻無法讀取", path.display()))),
            Err(e) => Err(Error::new(
                e.kind(),
                format!("無法建立 {}: {e}", path.display()),
            )),
        }
    }
}

/// RFC 4648 base32（小寫、不補 `=`）的前 `chars` 個字元
fn base32_lower(bytes: &[u8], chars: usize) -> String {
    const ALPHA: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(chars);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 && out.len() < chars {
            bits -= 5;
            out.push(ALPHA[((acc >> bits) & 31) as usize] as char);
        }
        acc &= (1 << bits) - 1;
        if out.len() >= chars {
            break;
        }
    }
    // 最後不足 5 位元的部分補 0（RFC 4648）
    if bits > 0 && out.len() < chars {
        out.push(ALPHA[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// IP 位址截到 /24（IPv4）或 /48（IPv6）的網段，例 `10.0.3.0/24`、`2001:db8:1::/48`
pub fn truncate_ip(ip: IpAddr) -> String {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}/24", Ipv4Addr::new(o[0], o[1], o[2], 0))
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{}/48", Ipv6Addr::new(s[0], s[1], s[2], 0, 0, 0, 0, 0))
        }
    }
}

/// 介面位址（CIDR，例 `192.168.10.21/24`）截成網段：前綴取原本與 /24（/48）中較短者
pub fn truncate_cidr(cidr: &str) -> Option<String> {
    let (ip, prefix) = cidr.split_once('/')?;
    let prefix: u8 = prefix.parse().ok()?;
    match ip.parse::<IpAddr>().ok()? {
        IpAddr::V4(v4) => {
            let p = prefix.min(24);
            let mask = if p == 0 {
                0
            } else {
                u32::MAX << (32 - u32::from(p))
            };
            Some(format!("{}/{p}", Ipv4Addr::from(u32::from(v4) & mask)))
        }
        IpAddr::V6(v6) => {
            let p = prefix.min(48);
            let mask = if p == 0 {
                0
            } else {
                u128::MAX << (128 - u32::from(p))
            };
            Some(format!("{}/{p}", Ipv6Addr::from(u128::from(v6) & mask)))
        }
    }
}

/// MAC 只留前 3 組（OUI，廠商碼），例 `52:54:00:00:00:00`
pub fn truncate_mac(mac: &str) -> String {
    let parts: Vec<&str> = mac.split(':').collect();
    if parts.len() != 6 {
        return String::new();
    }
    format!("{}:{}:{}:00:00:00", parts[0], parts[1], parts[2])
}

/// 遮蔽命令列中的秘密：`--password x`／`--password=x`（名稱含 password、token、secret 等，規則同診斷包的
/// [`kp_obs::redact::secret_key`]）、`-p<密碼>`（mysql 等的寫法）、`key=value`、Bearer、API／註冊 token、
/// 帶帳密或查詢字串的網址（[`kp_obs::redact::redact`]）
pub fn redact_command(cmd: &str) -> String {
    const MASK: &str = kp_obs::redact::MASK;
    let mut out: Vec<String> = Vec::new();
    let mut mask_next = false;
    for tok in cmd.split(' ') {
        if mask_next && !tok.is_empty() {
            out.push(MASK.to_string());
            mask_next = false;
            continue;
        }
        let flag = tok.trim_start_matches('-');
        let dashes = tok.len() - flag.len();
        if dashes > 0 && !flag.is_empty() {
            let lower = flag.to_ascii_lowercase();
            if let Some((name, _)) = flag.split_once('=') {
                if kp_obs::redact::secret_key(name) {
                    out.push(format!("{}{name}={MASK}", "-".repeat(dashes)));
                    continue;
                }
            } else if dashes == 1
                && flag.len() > 1
                && flag.starts_with('p')
                && !matches!(
                    lower.as_str(),
                    "password" | "passwd" | "passphrase" | "pass" | "pwd"
                )
            {
                // mysql -pSECRET、mysqldump -pSECRET（密碼直接接在 -p 後面）
                out.push(format!("-p{MASK}"));
                continue;
            } else if kp_obs::redact::secret_key(flag) || matches!(lower.as_str(), "pass" | "pwd") {
                // --password <值>、-password <值>
                out.push(tok.to_string());
                mask_next = true;
                continue;
            }
        }
        out.push(tok.to_string());
    }
    let joined = out.join(" ");
    let mut counts = kp_obs::redact::Counts::default();
    kp_obs::redact::redact(&joined, &mut counts)
}

/// 執行中套用的個資政策
#[derive(Debug, Default)]
pub struct PrivacyPolicy {
    pub settings: PrivacySettings,
    key: Option<PseudonymKey>,
}

impl PrivacyPolicy {
    /// 預設政策（全部保存、命令列遮蔽秘密；測試用）
    #[cfg(test)]
    pub fn shared_default() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    #[cfg(test)]
    pub fn new(settings: PrivacySettings, key: Option<PseudonymKey>) -> Self {
        Self { settings, key }
    }

    /// 服務啟動時：需要假名化時建立或讀取金鑰；無法取得時回錯誤（Kairos 不啟動：
    /// 管理者要求假名化，不能退回保存原值）
    pub fn for_service(settings: PrivacySettings, data_dir: &Path) -> Result<Self, Error> {
        let key = if settings.needs_key() {
            Some(PseudonymKey::load_or_create(data_dir).map_err(|e| {
                Error::new(
                    e.kind(),
                    format!(
                        "privacy.hostname 或 privacy.process_user 設為 pseudonymize，但無法建立或讀取假名化的金鑰檔 {}：{e}",
                        data_dir.join(KEY_FILE).display()
                    ),
                )
            })?)
        } else {
            None
        };
        Ok(Self { settings, key })
    }

    /// 註冊服務等其他元件：讀取既有的金鑰（服務啟動時已建立）
    pub fn load(settings: PrivacySettings, data_dir: &Path) -> Self {
        let key = if settings.needs_key() {
            match PseudonymKey::load(data_dir) {
                Ok(k) => k,
                Err(e) => {
                    log::error!("無法讀取假名化的金鑰檔：{e}");
                    None
                }
            }
        } else {
            None
        };
        Self { settings, key }
    }

    fn pseudonym(&self, kind: &str, value: &str) -> String {
        match &self.key {
            Some(k) => k.pseudonym(kind, value),
            // 沒有金鑰時（服務啟動時已確保有金鑰，這裡是防禦）：不保存原值
            None => format!("{kind}-unavailable"),
        }
    }

    /// 要保存的來源位址
    pub fn peer(&self, addr: SocketAddr) -> String {
        match self.settings.store_peer_ip {
            PeerIpMode::Full => addr.to_string(),
            PeerIpMode::Truncate => truncate_ip(addr.ip()),
            PeerIpMode::None => String::new(),
        }
    }

    /// 要保存的 IP（沒有埠；被拒的憑證等）；`none` 時不保存
    pub fn ip(&self, ip: IpAddr) -> Option<String> {
        match self.settings.store_peer_ip {
            PeerIpMode::Full => Some(ip.to_canonical().to_string()),
            PeerIpMode::Truncate => Some(truncate_ip(ip)),
            PeerIpMode::None => None,
        }
    }

    /// 要保存的主機名稱
    pub fn hostname(&self, name: &str) -> String {
        match self.settings.hostname {
            HostnameMode::Keep => name.to_string(),
            HostnameMode::Pseudonymize => self.pseudonym("host", name),
        }
    }

    /// 要保存的顯示名稱
    pub fn display_name(&self, name: Option<String>) -> Option<String> {
        match self.settings.hostname {
            HostnameMode::Keep => name,
            HostnameMode::Pseudonymize => name.map(|n| self.pseudonym("name", &n)),
        }
    }

    /// hello 的主機資訊（顯示名稱）
    pub fn host_info(&self, mut host: kairos_proto::HostInfo) -> kairos_proto::HostInfo {
        host.display_name = self.display_name(host.display_name.take());
        host
    }

    /// report 的行程排行
    pub fn processes(&self, list: &mut kairos_proto::ProcessList) {
        for e in list.top_cpu.iter_mut().chain(list.top_memory.iter_mut()) {
            e.command = match self.settings.process_command {
                ProcessCommandMode::Keep => e.command.take(),
                ProcessCommandMode::Redact => e.command.take().map(|c| redact_command(&c)),
                ProcessCommandMode::Drop => None,
            };
            e.user = match self.settings.process_user {
                ProcessUserMode::Keep => e.user.take(),
                ProcessUserMode::Pseudonymize => e.user.take().map(|u| self.pseudonym("user", &u)),
                ProcessUserMode::Drop => None,
            };
        }
    }

    /// `inventory` 的網路介面
    pub fn inventory(&self, inv: &mut kairos_proto::Inventory) {
        let Some(ifaces) = inv.network_interfaces.as_mut() else {
            return;
        };
        for i in ifaces {
            match self.settings.inventory_network {
                InventoryNetworkMode::Keep => {}
                InventoryNetworkMode::Truncate => {
                    let mut seen = Vec::new();
                    for a in &i.addresses {
                        if let Some(t) = truncate_cidr(a)
                            && !seen.contains(&t)
                        {
                            seen.push(t);
                        }
                    }
                    i.addresses = seen;
                    i.mac = i.mac.as_deref().map(truncate_mac).filter(|m| !m.is_empty());
                }
                InventoryNetworkMode::Drop => {
                    i.addresses.clear();
                    i.mac = None;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 資料主體存取請求（DSAR）：一台主機的個資匯出（`kairos hosts export`、`GET /api/v1/hosts/{agent_id}/personal-data`）

/// 匯出文件的 `schema`
pub const EXPORT_SCHEMA: &str = "kairos.host.personal-data/v1";

/// 匯出的上限（每一類）：告警事件、註冊申請、稽核紀錄
pub(crate) const EXPORT_MAX_ROWS: i64 = 10_000;

/// 時間欄位（Unix 毫秒）轉成 RFC 3339：名稱以 `_at`、`_until` 結尾，或是下列名稱
pub(crate) fn is_time_column(name: &str) -> bool {
    name.ends_with("_at")
        || name.ends_with("_until")
        || matches!(
            name,
            "first_seen" | "last_seen" | "not_before" | "not_after" | "ts" | "at"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> PseudonymKey {
        PseudonymKey::from_bytes(&[7u8; 32])
    }

    #[test]
    fn peer_addresses_are_kept_truncated_or_dropped() {
        let addr: SocketAddr = "10.0.3.27:50123".parse().unwrap();
        let v6: SocketAddr = "[2001:db8:1:2::5]:443".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:192.168.1.9]:1".parse().unwrap();
        let p = |m| {
            PrivacyPolicy::new(
                PrivacySettings {
                    store_peer_ip: m,
                    ..Default::default()
                },
                None,
            )
        };
        assert_eq!(p(PeerIpMode::Full).peer(addr), "10.0.3.27:50123");
        assert_eq!(p(PeerIpMode::Truncate).peer(addr), "10.0.3.0/24");
        assert_eq!(p(PeerIpMode::Truncate).peer(v6), "2001:db8:1::/48");
        assert_eq!(p(PeerIpMode::Truncate).peer(mapped), "192.168.1.0/24");
        assert_eq!(p(PeerIpMode::None).peer(addr), "");
    }

    #[test]
    fn host_pseudonyms_are_recognized_by_format() {
        let a = key().pseudonym("host", "his-db-01");
        assert!(is_host_pseudonym(&a), "{a}");
        for not in [
            "his-db-01",
            "host-abc",
            "host-ABCDEFGHIJKL",
            "host-abcdefghijk1",
            "host-abcdefghijklm",
            "name-abcdefghijkl",
            "",
        ] {
            assert!(!is_host_pseudonym(not), "{not}");
        }
    }

    #[test]
    fn pseudonyms_are_stable_case_insensitive_and_keyed() {
        let k = key();
        let a = k.pseudonym("host", "his-db-01");
        assert!(
            a.starts_with("host-") && a.len() == 5 + PSEUDONYM_CHARS,
            "{a}"
        );
        assert!(
            a[5..]
                .bytes()
                .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
        );
        assert_eq!(a, k.pseudonym("host", "HIS-DB-01 "));
        assert_ne!(a, k.pseudonym("host", "his-db-02"));
        assert_ne!(a, k.pseudonym("user", "his-db-01"), "不同種類不同假名");
        assert_ne!(
            a,
            PseudonymKey::from_bytes(&[8u8; 32]).pseudonym("host", "his-db-01"),
            "換金鑰後不同"
        );
        // 不可由假名猜出原值：與原值沒有共同的片段
        assert!(!a.contains("db"));
        let p = PrivacyPolicy::new(
            PrivacySettings {
                hostname: HostnameMode::Pseudonymize,
                ..Default::default()
            },
            Some(key()),
        );
        assert_eq!(p.hostname("his-db-01"), a);
        assert_eq!(
            p.display_name(Some("王小明的筆電".into())).unwrap(),
            k.pseudonym("name", "王小明的筆電")
        );
        let keep = PrivacyPolicy::default();
        assert_eq!(keep.hostname("x"), "x");
        assert_eq!(keep.display_name(Some("y".into())), Some("y".into()));
    }

    #[test]
    fn base32_matches_rfc4648() {
        // RFC 4648 第 10 節：BASE32("foobar") = "MZXW6YTBOI======"
        assert_eq!(base32_lower(b"foobar", 10), "mzxw6ytboi");
        assert_eq!(base32_lower(b"f", 2), "my");
    }

    #[test]
    fn command_lines_are_redacted() {
        assert_eq!(
            redact_command("mysql -uroot -pS3cret -h db"),
            "mysql -uroot -p[已遮蔽] -h db"
        );
        assert_eq!(
            redact_command("app --password hunter2 --port 80"),
            "app --password [已遮蔽] --port 80"
        );
        assert_eq!(
            redact_command("app --api-token=abc --config /etc/a.toml"),
            "app --api-token=[已遮蔽] --config /etc/a.toml"
        );
        let r = redact_command(
            "curl -H Authorization: Bearer xyz123 https://u:p@example.com/x?token=1",
        );
        assert!(!r.contains("xyz123") && !r.contains("u:p@"), "{r}");
        assert_eq!(redact_command("mysql -pSecret"), "mysql -p[已遮蔽]");
        assert_eq!(
            redact_command("svc -password hunter2"),
            "svc -password [已遮蔽]"
        );
        // 沒有秘密的命令列不變
        assert_eq!(
            redact_command("/usr/bin/postgres -D /var/lib/pg"),
            "/usr/bin/postgres -D /var/lib/pg"
        );
    }

    #[test]
    fn process_fields_follow_the_policy() {
        let list = || kairos_proto::ProcessList {
            top_cpu: vec![kairos_proto::ProcessEntry {
                pid: 1,
                name: "mysql".into(),
                cpu_usage: 1.0,
                memory: 1,
                user: Some("ann".into()),
                command: Some("mysql -pSecret".into()),
            }],
            top_memory: Vec::new(),
        };
        let mut l = list();
        PrivacyPolicy::default().processes(&mut l);
        assert_eq!(l.top_cpu[0].command.as_deref(), Some("mysql -p[已遮蔽]"));
        assert_eq!(l.top_cpu[0].user.as_deref(), Some("ann"));
        let p = PrivacyPolicy::new(
            PrivacySettings {
                process_command: ProcessCommandMode::Drop,
                process_user: ProcessUserMode::Pseudonymize,
                ..Default::default()
            },
            Some(key()),
        );
        let mut l = list();
        p.processes(&mut l);
        assert_eq!(l.top_cpu[0].command, None);
        assert_eq!(l.top_cpu[0].user, Some(key().pseudonym("user", "ann")));
        let mut l = list();
        PrivacyPolicy::new(
            PrivacySettings {
                process_command: ProcessCommandMode::Keep,
                process_user: ProcessUserMode::Drop,
                ..Default::default()
            },
            None,
        )
        .processes(&mut l);
        assert_eq!(l.top_cpu[0].command.as_deref(), Some("mysql -pSecret"));
        assert_eq!(l.top_cpu[0].user, None);
    }

    #[test]
    fn inventory_addresses_and_macs() {
        assert_eq!(
            truncate_cidr("192.168.10.21/24").unwrap(),
            "192.168.10.0/24"
        );
        assert_eq!(truncate_cidr("10.1.2.3/8").unwrap(), "10.0.0.0/8");
        assert_eq!(truncate_cidr("10.1.2.3/32").unwrap(), "10.1.2.0/24");
        assert_eq!(
            truncate_cidr("fe80::5054:ff:fe12:3456/64").unwrap(),
            "fe80::/48"
        );
        assert_eq!(truncate_cidr("x"), None);
        assert_eq!(truncate_mac("52:54:00:12:34:56"), "52:54:00:00:00:00");
        let inv = || kairos_proto::Inventory {
            network_interfaces: Some(vec![kairos_proto::InterfaceInventory {
                name: "eth0".into(),
                mac: Some("52:54:00:12:34:56".into()),
                addresses: vec!["192.168.10.21/24".into(), "192.168.10.22/24".into()],
                mtu: Some(1500),
            }]),
            ..Default::default()
        };
        let mut i = inv();
        PrivacyPolicy::default().inventory(&mut i);
        assert_eq!(i, inv());
        let mut i = inv();
        PrivacyPolicy::new(
            PrivacySettings {
                inventory_network: InventoryNetworkMode::Truncate,
                ..Default::default()
            },
            None,
        )
        .inventory(&mut i);
        let n = &i.network_interfaces.as_ref().unwrap()[0];
        assert_eq!(n.addresses, ["192.168.10.0/24"]);
        assert_eq!(n.mac.as_deref(), Some("52:54:00:00:00:00"));
        let mut i = inv();
        PrivacyPolicy::new(
            PrivacySettings {
                inventory_network: InventoryNetworkMode::Drop,
                ..Default::default()
            },
            None,
        )
        .inventory(&mut i);
        let n = &i.network_interfaces.as_ref().unwrap()[0];
        assert!(n.addresses.is_empty() && n.mac.is_none() && n.mtu == Some(1500));
    }

    #[test]
    fn key_file_is_created_once_with_0600() {
        let dir = std::env::temp_dir().join(format!("kp-privacy-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(PseudonymKey::load(&dir).unwrap().is_none());
        let a = PseudonymKey::load_or_create(&dir).unwrap();
        let b = PseudonymKey::load_or_create(&dir).unwrap();
        assert_eq!(a.key_id(), b.key_id());
        assert_eq!(a.pseudonym("host", "x"), b.pseudonym("host", "x"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(dir.join(KEY_FILE), b"short").unwrap();
        assert!(PseudonymKey::load(&dir).is_err());
        // 需要金鑰的政策在金鑰檔損毀時無法啟動
        assert!(
            PrivacyPolicy::for_service(
                PrivacySettings {
                    hostname: HostnameMode::Pseudonymize,
                    ..Default::default()
                },
                &dir
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_parse_from_toml_and_describe() {
        let s: PrivacySettings = toml_like(
            r#"{"store_peer_ip":"truncate","hostname":"pseudonymize","process_command":"drop","process_user":"drop","inventory_network":"truncate"}"#,
        );
        assert!(s.needs_key());
        let d = s.describe().unwrap();
        assert!(d.contains("網段") && d.contains("假名"), "{d}");
        assert_eq!(PrivacySettings::default().describe(), None);
        assert!(serde_json::from_str::<PrivacySettings>(r#"{"store_peer_ip":"half"}"#).is_err());
    }

    fn toml_like(json: &str) -> PrivacySettings {
        serde_json::from_str(json).unwrap()
    }
}
