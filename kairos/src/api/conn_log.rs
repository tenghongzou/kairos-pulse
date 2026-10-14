//! 每台主機最近的上線與離線紀錄（`GET /api/v1/hosts/{agent_id}/connections`；設計包 S05
//! 「連線與憑證」的連線紀錄、「斷線紀錄」的最小版本）。
//!
//! 只保存在記憶體中：由狀態表的變化（[`crate::registry::HostChange`]，與 SSE 的 `host.*` 事件同一個
//! 來源）累積，Kairos 重新啟動後從頭開始。每台主機最多 [`PER_HOST`] 筆，最多 [`MAX_HOSTS`] 台
//! （超過時捨棄最久沒有變化的主機）。完整、持久的連線紀錄（設計包 §24 #2）屬於之後的版本。

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use super::http::ts;
use crate::registry::{HostChange, HostChangeKind};

/// 每台主機保留的筆數
pub const PER_HOST: usize = 50;
/// 最多保留幾台主機的紀錄
pub const MAX_HOSTS: usize = 20_000;

/// 一筆上線或離線
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub at: DateTime<Utc>,
    pub online: bool,
    /// 離線原因（`kairos/API.md` 第 5.2 節的 `offline_reason`）
    pub reason: Option<&'static str>,
    pub ip: Option<String>,
}

impl Entry {
    pub fn json(&self) -> Value {
        json!({
            "at": ts(self.at),
            "event": if self.online { "online" } else { "offline" },
            "reason": self.reason,
            "ip": self.ip,
        })
    }
}

/// 所有主機的紀錄
#[derive(Debug, Default)]
pub struct ConnLog {
    inner: Mutex<HashMap<Arc<str>, VecDeque<Entry>>>,
}

impl ConnLog {
    /// 記錄一次狀態變化
    pub fn record(&self, change: &HostChange) {
        let ip = change
            .peer
            .parse::<std::net::SocketAddr>()
            .map(|a| a.ip().to_canonical().to_string())
            .ok();
        let entry = Entry {
            at: change.at,
            online: matches!(change.kind, HostChangeKind::Online),
            reason: match change.kind {
                HostChangeKind::Online => None,
                HostChangeKind::Offline(r) => Some(r.as_str()),
            },
            ip,
        };
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !m.contains_key(&change.agent_id) && m.len() >= MAX_HOSTS {
            // 捨棄最久沒有變化的主機
            if let Some(oldest) = m
                .iter()
                .min_by_key(|(_, v)| v.back().map(|e| e.at))
                .map(|(k, _)| k.clone())
            {
                m.remove(&oldest);
            }
        }
        let list = m.entry(change.agent_id.clone()).or_default();
        if list.len() >= PER_HOST {
            list.pop_front();
        }
        list.push_back(entry);
    }

    /// 一台主機的紀錄（新到舊）
    pub fn get(&self, agent_id: &str) -> Vec<Entry> {
        let m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        m.get(agent_id)
            .map(|v| v.iter().rev().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::OfflineReason;

    fn change(id: &str, kind: HostChangeKind, secs: i64) -> HostChange {
        HostChange {
            agent_id: Arc::from(id),
            hostname: Arc::from("h"),
            display_name: None,
            peer: Arc::from("10.0.0.5:5000"),
            kind,
            at: DateTime::from_timestamp(secs, 0).unwrap(),
        }
    }

    #[test]
    fn keeps_the_latest_entries_per_host_newest_first() {
        let log = ConnLog::default();
        for i in 0..(PER_HOST as i64 + 5) {
            let kind = if i % 2 == 0 {
                HostChangeKind::Online
            } else {
                HostChangeKind::Offline(OfflineReason::Timeout)
            };
            log.record(&change("a", kind, i));
        }
        let got = log.get("a");
        assert_eq!(got.len(), PER_HOST);
        assert_eq!(got[0].at.timestamp(), PER_HOST as i64 + 4);
        assert_eq!(got[0].ip.as_deref(), Some("10.0.0.5"));
        let offline = got.iter().find(|e| !e.online).unwrap();
        assert_eq!(offline.reason, Some("timeout"));
        assert_eq!(offline.json()["event"], "offline");
        assert!(log.get("b").is_empty());
    }
}
