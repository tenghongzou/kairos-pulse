//! 總覽（`kairos/API.md` 第 5.4 節；設計包 S03）：各狀態台數、上線數 24 小時、離線最久、
//! 資源使用前 10、系統健康的摘要。

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use super::ApiContext;
use super::health::{self, Level};
use super::hosts::{HostView, LONG_OFFLINE_SECS, Snapshot, Status, status_counts};
use super::http::{ApiError, ok, ts_ms};
use crate::rbac::HostAccess;

/// 每分鐘的上線數保留多久（24 小時）
pub const MINUTE_SAMPLES: usize = 24 * 60;

/// 每分鐘一筆：(時間, 線上, 總數)
#[derive(Debug, Default)]
pub struct MinuteSeries {
    samples: VecDeque<(i64, u64, u64)>,
}

impl MinuteSeries {
    pub fn push(&mut self, at_ms: i64, online: u64, total: u64) {
        if self.samples.len() >= MINUTE_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((at_ms, online, total));
    }

    /// `at_ms` 之前（含）最近的一筆
    pub fn at_or_before(&self, at_ms: i64) -> Option<(i64, u64, u64)> {
        self.samples.iter().rev().find(|s| s.0 <= at_ms).copied()
    }

    pub fn points(&self) -> Vec<Value> {
        self.samples
            .iter()
            .map(|(t, online, total)| json!({ "t": ts_ms(*t), "online": online, "total": total }))
            .collect()
    }
}

/// 索引中各狀態的台數
pub fn counts(snap: &Snapshot) -> HashMap<Status, u64> {
    let mut c = HashMap::new();
    for h in &snap.hosts {
        *c.entry(h.status).or_insert(0) += 1;
    }
    c
}

fn brief(h: &HostView, now_ms: i64) -> Value {
    json!({
        "agent_id": &*h.agent_id,
        "hostname": &*h.hostname,
        "display_name": h.display_name.as_deref(),
        "last_seen": ts_ms(h.last_seen),
        "offline_for_secs": h.offline_for_secs(now_ms),
        "offline_reason": h.offline_reason,
        "offline_reason_legacy": h.offline_reason.map(crate::registry::legacy_offline_reason),
        "cpu_usage": h.cpu(),
        "memory_used_ratio": h.mem_ratio(),
    })
}

/// 依 `key` 由大到小取前 `n` 台
fn top_by<'a>(
    hosts: &[&'a HostView],
    n: usize,
    filter: impl Fn(&HostView) -> bool,
    key: impl Fn(&HostView) -> Option<f64>,
) -> Vec<&'a HostView> {
    let mut list: Vec<(&'a HostView, f64)> = hosts
        .iter()
        .copied()
        .filter(|h| filter(h))
        .filter_map(|h| key(h).map(|k| (h, k)))
        .collect();
    list.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| a.0.agent_id.cmp(&b.0.agent_id))
    });
    list.into_iter().take(n).map(|(h, _)| h).collect()
}

/// `GET /api/v1/overview`
pub async fn get(
    ctx: &Arc<ApiContext>,
    access: &HostAccess,
    health_for: Option<&crate::rbac::Grants>,
    now: DateTime<Utc>,
) -> Result<hyper::Response<super::http::ApiBody>, ApiError> {
    let snap = ctx.hosts.snapshot(ctx).await?;
    let now_ms = now.timestamp_millis();
    let all = *access == HostAccess::All;
    let hosts: Vec<&HostView> = snap
        .hosts
        .iter()
        .filter(|h| access.allows(h.host_group.as_deref()))
        .collect();
    let mut c: HashMap<Status, u64> = HashMap::new();
    for h in &hosts {
        *c.entry(h.status).or_insert(0) += 1;
    }
    let online = c.get(&Status::Online).copied().unwrap_or(0);
    let offline = c.get(&Status::Offline).copied().unwrap_or(0);
    let total = hosts.len() as u64;
    let (series, change) = {
        let m = ctx.minutes.lock().unwrap_or_else(|e| e.into_inner());
        let change = m.at_or_before(now_ms - 3_600_000).map(|(_, o, t)| {
            let before_offline = t.saturating_sub(o);
            json!({
                "online": online as i64 - o as i64,
                "offline": offline as i64 - before_offline as i64,
            })
        });
        (m.points(), change)
    };
    // 每分鐘的上線數與投遞計數是全站的數字：群組範圍的身分看不到
    let (series, change) = if all {
        (series, change)
    } else {
        (Vec::new(), None)
    };
    let live = ctx.registry.live_view();
    let (d_online, durable, replaying) = health::delivery_counts(&live, std::time::Instant::now());
    let delivery =
        all.then(|| json!({ "online": d_online, "durable": durable, "replaying": replaying }));
    let longest = top_by(
        &hosts,
        5,
        |h| h.status == Status::Offline,
        |h| Some(-(h.last_seen as f64)),
    );
    let top_cpu = top_by(&hosts, 10, |h| h.status == Status::Online, |h| h.cpu());
    let top_mem = top_by(
        &hosts,
        10,
        |h| h.status == Status::Online,
        |h| h.mem_ratio(),
    );
    // 有 health:read 時；摘要依權限省略（與 GET /api/v1/system/health 相同）
    let health = match health_for {
        Some(g) => health::compute(ctx, now).await.ok().map(|h| h.redact(g)),
        None => None,
    };
    Ok(ok(
        now,
        json!({
            "status_counts": status_counts(&c),
            "total": total,
            "never_connected": hosts.iter().filter(|h| h.never_connected).count(),
            "long_offline": hosts.iter().filter(|h| h.offline_for_secs(now_ms).is_some_and(|s| s > LONG_OFFLINE_SECS)).count(),
            "change_1h": change,
            "online_series": { "interval_secs": 60, "points": series },
            "delivery": delivery,
            "longest_offline": longest.iter().map(|h| brief(h, now_ms)).collect::<Vec<_>>(),
            "top_cpu": top_cpu.iter().map(|h| brief(h, now_ms)).collect::<Vec<_>>(),
            "top_memory": top_mem.iter().map(|h| brief(h, now_ms)).collect::<Vec<_>>(),
            "health": health.map(|h| json!({
                "status": h.status,
                "items": h.items.iter().filter(|i| i.status != Level::Ok).map(|i| json!({
                    "id": i.id, "status": i.status, "product_code": i.product_code, "summary": i.summary,
                    "restricted": i.restricted,
                })).collect::<Vec<_>>(),
            })),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minute_series_is_bounded() {
        let mut m = MinuteSeries::default();
        for i in 0..(MINUTE_SAMPLES as i64 + 10) {
            m.push(i * 60_000, i as u64, 100);
        }
        assert_eq!(m.points().len(), MINUTE_SAMPLES);
        assert_eq!(m.at_or_before(65 * 60_000 + 30_000).unwrap().1, 65);
        assert_eq!(m.at_or_before(0), None);
    }
}
