//! 叢集連線（feature [`FEATURE_CLUSTER`]，PROTOCOL.md「叢集與排空」；HA 規格 `kairos/HA.md` 第 18 節）。
//!
//! - Pulse 支援多個伺服器位址與排空時在 hello 宣告 `cluster`；Kairos 只在 client 也宣告時才在
//!   `hello_ack.features` 列出（沿用 `renew` 的先例：沒有宣告的 client 收到的 `hello_ack` 與 0.8 完全相同）。
//! - 協商了 `cluster` 時 `hello_ack` 另帶 `server_node`（回應的節點名稱）與選用的 `cluster_id`，
//!   Kairos 可以送 `drain`（[`ServerMsg::Drain`](crate::ServerMsg::Drain)）要求 agent 改連其他節點。
//! - 協定版本與錯誤碼都不變；舊版 Pulse 收到不認得的 `type` 只會忽略，但規格上沒有協商時不送 `drain`。

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// 叢集連線：`hello_ack.server_node`／`cluster_id` 與排空訊息 `drain`
pub const FEATURE_CLUSTER: &str = "cluster";

/// `hello_ack.server_node` 的長度上限（bytes；與 Kairos 的 `cluster.node_id` 相同）
pub const MAX_SERVER_NODE_BYTES: usize = 63;

/// `drain.reason`：維運排空或節點關機（Kairos 優雅關機時也送這個值）
pub const DRAIN_REASON_NODE_DRAINING: &str = "node_draining";
/// `drain.reason`：之後的負載重新分配（目前的 Kairos 不送）
pub const DRAIN_REASON_REBALANCE: &str = "rebalance";

/// `drain.reconnect_within_ms` 的範圍（毫秒）。超出範圍時 client 夾在範圍內
pub const MIN_DRAIN_RECONNECT_MS: u32 = 1_000;
pub const MAX_DRAIN_RECONNECT_MS: u32 = 300_000;
/// `drain.reconnect_within_ms` 的下限預設值（毫秒）；缺漏或型別不符時 client 也用這個值
pub const DEFAULT_DRAIN_RECONNECT_MS: u32 = 10_000;
/// Kairos 依被排空的連線數拉長分散時間：每條連線加這麼多毫秒
pub const DRAIN_MS_PER_CONNECTION: u32 = 5;

/// client 收到 `drain` 後，等待在途的 report 收到回應的上限（毫秒；與「伺服器回應延遲最多 8 秒」相同）
pub const DRAIN_IN_FLIGHT_WAIT_MS: u64 = 8_000;

/// Kairos 送出的 `reconnect_within_ms`：`max(10000, 連線數 × 5)`，不超過 [`MAX_DRAIN_RECONNECT_MS`]
pub fn drain_reconnect_within_ms(connections: usize) -> u32 {
    let per = u64::from(DRAIN_MS_PER_CONNECTION).saturating_mul(connections as u64);
    let ms = per.max(u64::from(DEFAULT_DRAIN_RECONNECT_MS));
    u32::try_from(ms.min(u64::from(MAX_DRAIN_RECONNECT_MS))).unwrap_or(MAX_DRAIN_RECONNECT_MS)
}

/// client 實際使用的分散時間：缺漏（或型別不符）時 [`DEFAULT_DRAIN_RECONNECT_MS`]，否則夾在
/// [`MIN_DRAIN_RECONNECT_MS`]～[`MAX_DRAIN_RECONNECT_MS`]
pub fn effective_drain_reconnect_ms(value: Option<u32>) -> u32 {
    value
        .unwrap_or(DEFAULT_DRAIN_RECONNECT_MS)
        .clamp(MIN_DRAIN_RECONNECT_MS, MAX_DRAIN_RECONNECT_MS)
}

/// 合法的節點名稱（`hello_ack.server_node`、Kairos 的 `cluster.node_id`）：1～63 bytes，
/// 只有小寫英文字母、數字與 `-`，不以 `-` 開頭或結尾（同 DNS 標籤）
pub fn is_server_node(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SERVER_NODE_BYTES
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// 由主機名稱導出預設的節點名稱：轉成小寫、只取第一段（`.` 之前）、其他字元改為 `-`、
/// 去掉頭尾的 `-` 並截到 63 bytes；結果不合法（例如主機名稱是空字串）時為 `kairos`
pub fn node_name_from_hostname(hostname: &str) -> String {
    let first = hostname.trim().split('.').next().unwrap_or("");
    let mapped: String = first
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut name: String = mapped.trim_matches('-').to_string();
    name.truncate(MAX_SERVER_NODE_BYTES);
    let name = name.trim_end_matches('-').to_string();
    if is_server_node(&name) {
        name
    } else {
        "kairos".to_string()
    }
}

/// 寬鬆的 `server_node`：不是字串或不符 [`is_server_node`] 時視為沒有（整則 `hello_ack` 仍可解析）
pub(crate) fn de_lenient_server_node<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) if is_server_node(&s) => Some(s),
        _ => None,
    })
}

/// 寬鬆的 `cluster_id`：不是 UUID 文字時視為沒有；轉成小寫
pub(crate) fn de_lenient_cluster_id<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) if crate::is_uuid_text(&s) => Some(s.to_ascii_lowercase()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_window_grows_with_connections_and_is_capped() {
        assert_eq!(drain_reconnect_within_ms(0), 10_000);
        assert_eq!(drain_reconnect_within_ms(2_000), 10_000);
        assert_eq!(drain_reconnect_within_ms(2_001), 10_005);
        assert_eq!(drain_reconnect_within_ms(10_000), 50_000);
        assert_eq!(drain_reconnect_within_ms(1_000_000), MAX_DRAIN_RECONNECT_MS);
        assert_eq!(
            drain_reconnect_within_ms(usize::MAX),
            MAX_DRAIN_RECONNECT_MS
        );
    }

    #[test]
    fn client_clamps_the_reconnect_window() {
        assert_eq!(
            effective_drain_reconnect_ms(None),
            DEFAULT_DRAIN_RECONNECT_MS
        );
        assert_eq!(
            effective_drain_reconnect_ms(Some(0)),
            MIN_DRAIN_RECONNECT_MS
        );
        assert_eq!(effective_drain_reconnect_ms(Some(5_000)), 5_000);
        assert_eq!(
            effective_drain_reconnect_ms(Some(u32::MAX)),
            MAX_DRAIN_RECONNECT_MS
        );
    }

    #[test]
    fn server_node_names() {
        for ok in ["kairos-a", "a", "node1", &"x".repeat(63)] {
            assert!(is_server_node(ok), "{ok}");
        }
        for bad in [
            "",
            "Kairos",
            "a_b",
            "a.b",
            "-a",
            "a-",
            &"x".repeat(64),
            "節點",
        ] {
            assert!(!is_server_node(bad), "{bad}");
        }
        assert_eq!(node_name_from_hostname("Kairos-A.example.com"), "kairos-a");
        assert_eq!(node_name_from_hostname("WIN_SRV01"), "win-srv01");
        assert_eq!(node_name_from_hostname("  "), "kairos");
        assert_eq!(node_name_from_hostname("--"), "kairos");
        assert_eq!(node_name_from_hostname("伺服器"), "kairos");
        let long = format!("{}-{}", "a".repeat(62), "b".repeat(10));
        let derived = node_name_from_hostname(&long);
        assert!(is_server_node(&derived) && derived.len() <= 63, "{derived}");
    }
}
