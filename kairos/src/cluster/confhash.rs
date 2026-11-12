//! 需要一致的設定的雜湊（`kairos/HA.md` 第 15.2 節）。
//!
//! 叢集中的節點必須使用相同的設定，「節點本機」的鍵除外（[`is_node_local`]）。每個節點啟動時計算其餘
//! 設定的雜湊寫入 `cluster_nodes.config_hash`；與 leader 不同時記 `KRS-W8505`（只警告：滾動修改設定的
//! 期間本來就會不一致）。

use serde_json::Value;
use std::collections::BTreeMap;

use crate::config::AppConfig;

/// 這個設定鍵屬於節點本機（不納入雜湊）：
///
/// - `cluster.node_id`、`cluster.advertise`；
/// - 所有監聽位址（`*.listen`）；
/// - `storage.data_dir`、`log.*`；
/// - 檔案路徑：憑證與私鑰（`*.cert_path`、`*.key_path`）、`auth.client_ca`、`auth.crl*`，以及其他
///   以 `_file`、`_path`、`_dir` 結尾的鍵（密碼檔、token 檔、CA 檔）
pub fn is_node_local(key: &str) -> bool {
    let last = key.rsplit('.').next().unwrap_or(key);
    matches!(
        key,
        "cluster.node_id" | "cluster.advertise" | "storage.data_dir"
    ) || key.starts_with("log.")
        || last == "listen"
        || last.ends_with("_file")
        || last.ends_with("_path")
        || last.ends_with("_dir")
        || key == "auth.client_ca"
        || key.starts_with("auth.crl")
}

fn flatten(prefix: &str, value: &Value, out: &mut BTreeMap<String, String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(&key, v, out);
            }
        }
        other => {
            out.insert(prefix.to_string(), other.to_string());
        }
    }
}

/// 需要一致的設定（鍵 → JSON 值的文字），依鍵排序
pub fn shared_settings(config: &AppConfig) -> BTreeMap<String, String> {
    let mut flat = BTreeMap::new();
    flatten(
        "",
        &serde_json::to_value(config).unwrap_or(Value::Null),
        &mut flat,
    );
    flat.retain(|k, _| !is_node_local(k));
    flat
}

/// 需要一致的設定的雜湊（SHA-256 的前 16 bytes，hex）
pub fn config_hash(config: &AppConfig) -> String {
    use std::fmt::Write;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    for (k, v) in shared_settings(config) {
        ctx.update(k.as_bytes());
        ctx.update(b"=");
        ctx.update(v.as_bytes());
        ctx.update(b"\n");
    }
    ctx.finish().as_ref()[..16]
        .iter()
        .fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_local_keys_do_not_change_the_hash() {
        let a = AppConfig::default();
        let mut b = AppConfig::default();
        b.cluster.node_id = "kairos-b".into();
        b.cluster.advertise = "10.0.0.2".into();
        b.server.listen = "10.0.0.2:8443".into();
        b.console.listen = "10.0.0.2:9443".into();
        b.observability.listen = "10.0.0.2:9464".into();
        b.storage.data_dir = "/srv/kairos-b".into();
        b.server.cert_path = "/etc/kairos/b.crt".into();
        b.server.key_path = "/etc/kairos/b.key".into();
        b.storage.postgres.password_file = "/etc/kairos/b.pw".into();
        b.storage.postgres.ca_file = "/etc/kairos/ca-b.pem".into();
        b.log.level = "debug".into();
        assert_eq!(config_hash(&a), config_hash(&b));
        assert_eq!(config_hash(&a).len(), 32);
    }

    #[test]
    fn shared_settings_change_the_hash() {
        let a = AppConfig::default();
        let mut b = AppConfig::default();
        b.cluster.lease_ttl_secs = 30;
        assert_ne!(config_hash(&a), config_hash(&b));
        let mut c = AppConfig::default();
        c.storage.raw_retention_days += 1;
        assert_ne!(config_hash(&a), config_hash(&c));
        let mut d = AppConfig::default();
        d.storage.postgres.host = "db2.example.com".into();
        assert_ne!(config_hash(&a), config_hash(&d));
    }

    /// 遠端管理（0.10.0）：雙人覆核的開關與升級時段的預設時區是叢集的行為，節點之間必須一致
    /// （不一致時 `KRS-W8505`；否則換手後送出的推送是否需要核准會跟著 leader 改變）。版本庫的目錄是
    /// 節點本機的路徑
    #[test]
    fn remote_approval_and_timezone_are_shared_but_the_releases_dir_is_node_local() {
        let a = AppConfig::default();
        let mut b = AppConfig::default();
        b.remote.rollout_approval = "required".into();
        assert_ne!(config_hash(&a), config_hash(&b));
        let mut c = AppConfig::default();
        c.alerts.timezone = "+09:00".into();
        assert_ne!(config_hash(&a), config_hash(&c));
        let mut d = AppConfig::default();
        d.remote.releases_dir = "/srv/kairos-b/releases".into();
        assert_eq!(config_hash(&a), config_hash(&d));
        assert!(is_node_local("remote.releases_dir"));
        assert!(!is_node_local("remote.rollout_approval"));
        assert!(!is_node_local("remote.allowed_signers"));
    }

    #[test]
    fn classification_of_keys() {
        for k in [
            "cluster.node_id",
            "server.listen",
            "enroll.listen",
            "log.level",
            "storage.data_dir",
            "console.cert_path",
            "storage.postgres.password_file",
            "observability.metrics_token_file",
            "auth.client_ca",
            "auth.crl",
        ] {
            assert!(is_node_local(k), "{k}");
        }
        for k in [
            "cluster.enabled",
            "cluster.lease_ttl_secs",
            "storage.backend",
            "storage.postgres.host",
            "alerts.enabled",
            "server.max_connections",
        ] {
            assert!(!is_node_local(k), "{k}");
        }
    }
}
