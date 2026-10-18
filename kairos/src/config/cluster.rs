//! `[cluster]`：叢集的節點設定（`kairos/HA.md` 第 15.1 節）。
//!
//! 0.9.0 只有 `node_id`：協定 feature `cluster` 的 `hello_ack.server_node`（Pulse 據此顯示連到哪個節點）。
//! 叢集模式本身（`enabled`、租約、PostgreSQL 等其餘鍵）留待之後的版本，加入時沿用同一個區段。

use serde::{Deserialize, Serialize};
use std::io::{Error, ErrorKind};

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ClusterSettings {
    /// 節點名稱：1～63 個字元的 `[a-z0-9-]`，不以 `-` 開頭或結尾。空字串（預設）時由主機名稱導出
    /// （第一段、轉成小寫、其他字元改為 `-`；見 [`kairos_proto::node_name_from_hostname`]）
    pub node_id: String,
}

impl ClusterSettings {
    pub fn validate(&self) -> Result<(), Error> {
        let id = self.node_id.trim();
        if !id.is_empty() && !kairos_proto::is_server_node(id) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "cluster.node_id「{id}」不合法：必須是 1～{} 個字元的小寫英文字母、數字或 -，且不以 - 開頭或結尾（留空時使用主機名稱）",
                    kairos_proto::MAX_SERVER_NODE_BYTES
                ),
            ));
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_defaults_to_the_hostname() {
        let c = ClusterSettings::default();
        assert!(c.validate().is_ok());
        assert_eq!(c.effective_node_id("Kairos-B.example.com"), "kairos-b");
        let c = ClusterSettings {
            node_id: " kairos-a ".to_string(),
        };
        assert!(c.validate().is_ok());
        assert_eq!(c.effective_node_id("ignored"), "kairos-a");
    }

    #[test]
    fn invalid_node_ids_are_rejected() {
        for bad in ["Kairos", "a_b", "-a", "a.b", &"x".repeat(64)] {
            let c = ClusterSettings {
                node_id: bad.to_string(),
            };
            let e = c.validate().unwrap_err().to_string();
            assert!(e.contains("cluster.node_id"), "{bad}: {e}");
        }
    }
}
