//! `[cluster]`：叢集的節點設定（`kairos/HA.md` 第 15.1 節）。
//!
//! 0.9.0 只有 `node_id`（協定 feature `cluster` 的 `hello_ack.server_node`）。0.10.0 加入叢集模式
//! （階段 A：主動／備援）：`enabled`、`mode`、`service_name`、`advertise`、`lease_ttl_secs`、
//! `heartbeat_secs`、`node_lost_secs`。

use serde::{Deserialize, Serialize};
use std::io::{Error, ErrorKind};
use std::time::Duration;

/// 階段 A：主動／備援
pub const MODE_ACTIVE_PASSIVE: &str = "active_passive";
/// 階段 B：多主動連線層（尚未實作）
pub const MODE_ACTIVE_ACTIVE: &str = "active_active";

/// leader 在「最後一次續約成功」之後超過 `lease_ttl_secs − 這個值` 仍沒有續約成功時自我降級
/// （`kairos/HA.md` 第 6.2 節）
pub const SELF_DEMOTE_MARGIN: Duration = Duration::from_secs(5);
/// 非 leader 嘗試取得租約的間隔
pub const ACQUIRE_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ClusterSettings {
    /// 叢集模式（需要 `storage.backend = "postgres"`）
    pub enabled: bool,
    /// `active_passive`（階段 A）；`active_active`（階段 B）尚未實作
    pub mode: String,
    /// 節點名稱：1～63 個字元的 `[a-z0-9-]`，不以 `-` 開頭或結尾。空字串（預設）時由主機名稱導出
    /// （第一段、轉成小寫、其他字元改為 `-`；見 [`kairos_proto::node_name_from_hostname`]）
    pub node_id: String,
    /// 選用：服務名稱（例 `kairos.example.com`）；`--check-config` 檢查伺服器憑證含這個 SAN
    pub service_name: String,
    /// 選用：顯示用的位址（`kairos cluster status`、系統健康）
    pub advertise: String,
    /// leader 租約的存活時間（秒，5..=120）；應大於資料庫自動切換所需的時間
    pub lease_ttl_secs: u64,
    /// 節點心跳的間隔（秒，1..=30）
    pub heartbeat_secs: u64,
    /// 心跳超過這個時間的節點視為失聯（秒；至少 3 × `heartbeat_secs`）
    pub node_lost_secs: u64,
}

impl Default for ClusterSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: MODE_ACTIVE_PASSIVE.to_string(),
            node_id: String::new(),
            service_name: String::new(),
            advertise: String::new(),
            lease_ttl_secs: 15,
            heartbeat_secs: 5,
            node_lost_secs: 20,
        }
    }
}

impl ClusterSettings {
    pub fn validate(&self) -> Result<(), Error> {
        let invalid = |msg: String| Err(Error::new(ErrorKind::InvalidInput, msg));
        let id = self.node_id.trim();
        if !id.is_empty() && !kairos_proto::is_server_node(id) {
            return invalid(format!(
                "cluster.node_id「{id}」不合法：必須是 1～{} 個字元的小寫英文字母、數字或 -，且不以 - 開頭或結尾（留空時使用主機名稱）",
                kairos_proto::MAX_SERVER_NODE_BYTES
            ));
        }
        match self.mode.as_str() {
            MODE_ACTIVE_PASSIVE => {}
            MODE_ACTIVE_ACTIVE if self.enabled => {
                return invalid(
                    "cluster.mode = \"active_active\"（多主動連線層，kairos/HA.md 階段 B）尚未實作；請使用 \"active_passive\"".to_string(),
                );
            }
            MODE_ACTIVE_ACTIVE => {}
            other => {
                return invalid(format!(
                    "cluster.mode「{other}」不合法：必須是 \"active_passive\"（\"active_active\" 尚未實作）"
                ));
            }
        }
        if !(5..=120).contains(&self.lease_ttl_secs) {
            return invalid(format!(
                "cluster.lease_ttl_secs 必須介於 5..=120（目前為 {}）",
                self.lease_ttl_secs
            ));
        }
        if !(1..=30).contains(&self.heartbeat_secs) {
            return invalid(format!(
                "cluster.heartbeat_secs 必須介於 1..=30（目前為 {}）",
                self.heartbeat_secs
            ));
        }
        if self.node_lost_secs < 3 * self.heartbeat_secs || self.node_lost_secs > 3600 {
            return invalid(format!(
                "cluster.node_lost_secs 必須至少是 heartbeat_secs 的 3 倍（{}）且不超過 3600（目前為 {}）",
                3 * self.heartbeat_secs,
                self.node_lost_secs
            ));
        }
        let name = self.service_name.trim();
        if name.len() > 253 || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return invalid(format!("cluster.service_name「{name}」不合法"));
        }
        let adv = self.advertise.trim();
        if adv.len() > 255 || adv.chars().any(char::is_control) {
            return invalid(
                "cluster.advertise 不合法（最多 255 個字元，不可有控制字元）".to_string(),
            );
        }
        Ok(())
    }

    /// 實際使用的節點名稱：設定值，或由 `hostname` 導出
    pub fn effective_node_id(&self, hostname: &str) -> String {
        match self.node_id.trim() {
            "" => kairos_proto::node_name_from_hostname(hostname),
            id => id.to_string(),
        }
    }

    pub fn lease_ttl(&self) -> Duration {
        Duration::from_secs(self.lease_ttl_secs)
    }

    /// leader 續約的間隔：TTL 的 1/3
    pub fn renew_interval(&self) -> Duration {
        self.lease_ttl() / 3
    }

    /// 續約成功後多久沒有再成功就自我降級：`TTL − 5 秒`（TTL 是 5 秒時為 TTL 的一半）
    pub fn self_demote_after(&self) -> Duration {
        let ttl = self.lease_ttl();
        ttl.checked_sub(SELF_DEMOTE_MARGIN)
            .filter(|d| *d >= ttl / 2)
            .unwrap_or(ttl / 2)
    }

    pub fn heartbeat_interval(&self) -> Duration {
        Duration::from_secs(self.heartbeat_secs)
    }

    pub fn node_lost_after(&self) -> Duration {
        Duration::from_secs(self.node_lost_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_defaults_to_the_hostname() {
        let c = ClusterSettings::default();
        assert!(c.validate().is_ok());
        assert!(!c.enabled);
        assert_eq!(c.effective_node_id("Kairos-B.example.com"), "kairos-b");
        let c = ClusterSettings {
            node_id: " kairos-a ".to_string(),
            ..ClusterSettings::default()
        };
        assert!(c.validate().is_ok());
        assert_eq!(c.effective_node_id("ignored"), "kairos-a");
    }

    #[test]
    fn invalid_node_ids_are_rejected() {
        for bad in ["Kairos", "a_b", "-a", "a.b", &"x".repeat(64)] {
            let c = ClusterSettings {
                node_id: bad.to_string(),
                ..ClusterSettings::default()
            };
            let e = c.validate().unwrap_err().to_string();
            assert!(e.contains("cluster.node_id"), "{bad}: {e}");
        }
    }

    #[test]
    fn timing_follows_the_spec_defaults() {
        // HA.md 第 6.2 節：TTL 15 秒、續約間隔為 TTL 的 1/3、TTL − 5 秒沒有續約成功就自我降級
        let c = ClusterSettings::default();
        assert_eq!(c.lease_ttl(), Duration::from_secs(15));
        assert_eq!(c.renew_interval(), Duration::from_secs(5));
        assert_eq!(c.self_demote_after(), Duration::from_secs(10));
        assert_eq!(c.node_lost_after(), Duration::from_secs(20));
        // TTL 很短時不會變成 0：至少是 TTL 的一半
        let short = ClusterSettings {
            lease_ttl_secs: 5,
            ..ClusterSettings::default()
        };
        assert_eq!(short.self_demote_after(), Duration::from_millis(2500));
        let eight = ClusterSettings {
            lease_ttl_secs: 8,
            ..ClusterSettings::default()
        };
        assert_eq!(eight.self_demote_after(), Duration::from_secs(4));
    }

    #[test]
    fn ranges_and_mode_are_checked() {
        let bad = |f: fn(&mut ClusterSettings), what: &str| {
            let mut c = ClusterSettings::default();
            f(&mut c);
            let e = c.validate().unwrap_err().to_string();
            assert!(e.contains(what), "{what}: {e}");
        };
        bad(|c| c.lease_ttl_secs = 4, "lease_ttl_secs");
        bad(|c| c.lease_ttl_secs = 121, "lease_ttl_secs");
        bad(|c| c.heartbeat_secs = 0, "heartbeat_secs");
        bad(|c| c.node_lost_secs = 14, "node_lost_secs");
        bad(|c| c.mode = "x".into(), "cluster.mode");
        bad(
            |c| {
                c.enabled = true;
                c.mode = MODE_ACTIVE_ACTIVE.into();
            },
            "尚未實作",
        );
    }
}
