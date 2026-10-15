//! 規則的歷史試算（S09「以過去 7 天的資料試算：會觸發 N 次」；設計包 §24 #9）。
//!
//! - **指標門檻**：以 5 分鐘彙總（`rollup_5m` 的平均值）重播規則：連續的桶都符合觸發條件、且合計達到
//!   「持續 N 分鐘」時算一次觸發；符合恢復條件達到恢復時間時結束。資料以 5 分鐘為單位，持續時間不足
//!   5 分鐘的條件以 5 分鐘計（`approximate: true`）。缺資料的桶（離線、儲存失敗）中斷連續。
//!   範圍超過 [`MAX_HOSTS_5M`] 台時改用 1 小時彙總。
//! - **主機離線、長期失聯、回報延遲、身分衝突**：Kairos 沒有保存每次斷線的歷史（設計包 §24 #2 的連線
//!   紀錄屬於之後的版本），回 `supported: false` 並說明原因，不提供不準確的數字。

use chrono::FixedOffset;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::model::Rule;
use super::rules::kind;
use super::scope::{Compiled, HostFacts};

/// 以 5 分鐘彙總試算的主機數上限（超過時改用 1 小時彙總）
pub const MAX_HOSTS_5M: usize = 2000;
/// 試算最多的天數
pub const MAX_DAYS: u32 = 31;

/// 一台主機的一個桶：(桶的開始秒, 值)
type Series = Vec<(i64, f64)>;

fn series_ids(conn: &Connection, name: &str) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn
        .prepare("SELECT series_id FROM series WHERE name = ?1 AND labels = '' AND derived = 0")?;
    let rows = stmt.query_map([name], |r| r.get(0))?;
    rows.collect()
}

/// 這台主機某個名稱的序列（有 label 的指標，例如每個掛載點）：(series_id, label 的值, derived)
fn labeled_series(
    conn: &Connection,
    host_id: i64,
    name: &str,
    label: &str,
) -> rusqlite::Result<Vec<(i64, String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT s.series_id, s.labels, s.derived FROM series s JOIN host_series h USING (series_id)
         WHERE h.host_id = ?1 AND s.name = ?2",
    )?;
    let rows = stmt.query_map(params![host_id, name], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, labels, derived) = row?;
        let map: BTreeMap<String, String> = serde_json::from_str(&labels).unwrap_or_default();
        if let Some(v) = map.get(label) {
            out.push((id, v.clone(), derived));
        }
    }
    Ok(out)
}

/// 一台主機在試算期間的資料：每個對象（磁碟為掛載點、網路為介面；其他為一個）一條等間隔的序列
fn host_data(
    conn: &Connection,
    table: &str,
    host_id: i64,
    metric: &str,
    params_v: &Value,
    from: i64,
    std_ids: &(Vec<i64>, Vec<i64>),
) -> rusqlite::Result<Vec<Series>> {
    use super::rules::metric as m;
    let ratio = |a: BTreeMap<i64, f64>, b: &BTreeMap<i64, f64>, scale: f64| -> Series {
        a.iter()
            .filter_map(|(t, u)| {
                b.get(t)
                    .filter(|tot| **tot > 0.0)
                    .map(|tot| (*t, u / tot * scale))
            })
            .collect()
    };
    let in_scope = |key: &str, name: &str| {
        super::rules::globs(params_v, key)
            .is_none_or(|g| g.iter().any(|p| super::rules::glob_match(p, name)))
    };
    Ok(match metric {
        m::MEMORY | m::SWAP => {
            let a = load_series(conn, table, host_id, &std_ids.0, from)?;
            let b = load_series(conn, table, host_id, &std_ids.1, from)?;
            vec![ratio(a, &b, 100.0)]
        }
        m::LOAD => {
            let cores: Option<i64> = conn
                .query_row(
                    "SELECT cpu_logical_cores FROM host_inventory WHERE host_id = ?1",
                    [host_id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            match cores.filter(|c| *c > 0) {
                Some(c) => vec![
                    load_series(conn, table, host_id, &std_ids.0, from)?
                        .into_iter()
                        .map(|(t, v)| (t, v / c as f64))
                        .collect(),
                ],
                None => Vec::new(),
            }
        }
        m::DISK => {
            // 每個掛載點：Pulse 送的使用率優先，沒有時用推算值（0.2.0～0.7.0 的 Pulse）
            let mut by_mount: BTreeMap<String, BTreeMap<i64, f64>> = BTreeMap::new();
            let mut list = labeled_series(
                conn,
                host_id,
                kairos_proto::METRIC_FILESYSTEM_USAGE,
                "mountpoint",
            )?;
            list.sort_by_key(|(_, _, derived)| std::cmp::Reverse(*derived));
            for (id, mount, _) in list {
                if !in_scope("mountpoints", &mount) {
                    continue;
                }
                let data = load_series(conn, table, host_id, &[id], from)?;
                by_mount.entry(mount).or_default().extend(data);
            }
            by_mount
                .into_values()
                .map(|d| d.into_iter().collect())
                .collect()
        }
        m::NETWORK_RX | m::NETWORK_TX => {
            let name = if metric == m::NETWORK_RX {
                kairos_proto::METRIC_NETWORK_RECEIVED
            } else {
                kairos_proto::METRIC_NETWORK_TRANSMITTED
            };
            let mut out = Vec::new();
            for (id, dev, derived) in labeled_series(conn, host_id, name, "device")? {
                if derived != crate::store::derive::DERIVED_RATE || !in_scope("devices", &dev) {
                    continue;
                }
                out.push(
                    load_series(conn, table, host_id, &[id], from)?
                        .into_iter()
                        .map(|(t, v)| (t, v * 8.0 / 1_000_000.0))
                        .collect(),
                );
            }
            out
        }
        _ => vec![
            load_series(conn, table, host_id, &std_ids.0, from)?
                .into_iter()
                .collect(),
        ],
    })
}

fn load_series(
    conn: &Connection,
    table: &str,
    host_id: i64,
    series: &[i64],
    from_secs: i64,
) -> rusqlite::Result<BTreeMap<i64, f64>> {
    let mut out = BTreeMap::new();
    for s in series {
        let mut stmt = conn.prepare(&format!(
            "SELECT bucket, vsum / cnt FROM {table} WHERE host_id = ?1 AND bucket >= ?2 AND series_id = ?3 AND cnt > 0"
        ))?;
        let rows = stmt.query_map(params![host_id, from_secs, s], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
        })?;
        for row in rows {
            let (b, v) = row?;
            out.insert(b, v);
        }
    }
    Ok(out)
}

/// 指標門檻的重播結果
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Replay {
    /// 每次觸發：(觸發的時間秒, 持續秒數；到最後仍未恢復為 `None`)
    pub fires: Vec<(i64, Option<i64>)>,
}

/// 以等間隔的桶重播（`step` 為桶長秒數）
pub fn replay(
    data: &Series,
    step: i64,
    op: &str,
    threshold: f64,
    for_secs: i64,
    clear: Option<f64>,
    clear_for_secs: i64,
) -> Replay {
    let cmp = |v: f64| match op {
        ">" => v > threshold,
        ">=" => v >= threshold,
        "<" => v < threshold,
        _ => v <= threshold,
    };
    let cleared = |v: f64| match clear {
        Some(c) => {
            if op.starts_with('>') {
                v < c
            } else {
                v > c
            }
        }
        None => !cmp(v),
    };
    let mut out = Replay::default();
    let mut streak_start: Option<i64> = None;
    let mut clear_start: Option<i64> = None;
    let mut firing: Option<i64> = None;
    let mut prev: Option<i64> = None;
    for &(t, v) in data {
        // 缺資料：中斷連續（觸發中的事件維持，恢復的連續也中斷）
        if prev.is_some_and(|p| t - p > step) {
            streak_start = None;
            clear_start = None;
        }
        prev = Some(t);
        match firing {
            None => {
                if cmp(v) {
                    let s = *streak_start.get_or_insert(t);
                    // 一個桶代表 step 秒的資料
                    if t + step - s >= for_secs.max(step) {
                        firing = Some(t + step);
                        clear_start = None;
                    }
                } else {
                    streak_start = None;
                }
            }
            Some(at) => {
                if cleared(v) {
                    let s = *clear_start.get_or_insert(t);
                    if t + step - s >= clear_for_secs.max(step) {
                        out.fires.push((at, Some(t + step - at)));
                        firing = None;
                        streak_start = None;
                        clear_start = None;
                    }
                } else {
                    clear_start = None;
                }
            }
        }
    }
    if let Some(at) = firing {
        out.fires.push((at, None));
    }
    out
}

/// 試算一條規則（`rule` 已驗證）。`hosts` 為清冊的（host_id, 比對資訊）
pub fn preview(
    conn: &Connection,
    rule: &Rule,
    days: u32,
    now_ms: i64,
    offset: FixedOffset,
) -> rusqlite::Result<Value> {
    let days = days.clamp(1, MAX_DAYS);
    if rule.kind != kind::METRIC_THRESHOLD {
        return Ok(json!({
            "supported": false,
            "reason": "這種規則需要每次斷線與連線的歷史；Kairos 目前只保存最近一次的狀態，無法以過去的資料試算（之後的版本加入連線紀錄後提供）",
        }));
    }
    let p = &rule.params;
    let metric = p.get("metric").and_then(Value::as_str).unwrap_or("cpu");
    let op = p.get("op").and_then(Value::as_str).unwrap_or(">");
    let threshold = p.get("threshold").and_then(Value::as_f64).unwrap_or(90.0);
    let for_secs = p.get("for_secs").and_then(Value::as_i64).unwrap_or(600);
    let clear = p.get("clear_threshold").and_then(Value::as_f64);
    let clear_for = p.get("clear_for_secs").and_then(Value::as_i64).unwrap_or(0);
    let scope = Compiled::new(&rule.scope);
    // 範圍內的主機
    let mut stmt = conn.prepare(
        "SELECT host_id, agent_id, tags, peer, hostname, display_name FROM hosts WHERE lifecycle = 'active'",
    )?;
    type Row5 = (i64, String, Option<String>, Option<String>, String);
    let hosts: Vec<(i64, String, String)> = stmt
        .query_map([], |r| -> rusqlite::Result<Row5> {
            let tags: Option<String> = r.get(2)?;
            let peer: Option<String> = r.get(3)?;
            let name: String = r
                .get::<_, Option<String>>(5)?
                .unwrap_or(r.get::<_, String>(4)?);
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                tags,
                peer,
                name,
            ))
        })?
        .filter_map(Result::ok)
        .filter(|(_, agent, tags, peer, _)| {
            scope.matches(&HostFacts {
                agent_id: agent.clone(),
                tags: tags
                    .as_deref()
                    .and_then(|t| serde_json::from_str::<Vec<String>>(t).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| t.to_lowercase())
                    .collect(),
                ip: peer.as_deref().and_then(crate::bus::peer_ip),
            })
        })
        .map(|(id, agent, _, _, name)| (id, agent, name))
        .collect();
    let (table, step) = if hosts.len() > MAX_HOSTS_5M {
        ("rollup_1h", 3600)
    } else {
        ("rollup_5m", 300)
    };
    let from = now_ms / 1000 - i64::from(days) * 86_400;
    use super::rules::metric as m;
    let std_ids = match metric {
        m::MEMORY => (
            series_ids(conn, crate::protocol::METRIC_MEMORY_USED)?,
            series_ids(conn, crate::protocol::METRIC_MEMORY_TOTAL)?,
        ),
        m::SWAP => (
            series_ids(conn, kairos_proto::METRIC_SWAP_USED)?,
            series_ids(conn, kairos_proto::METRIC_SWAP_TOTAL)?,
        ),
        m::LOAD => (
            series_ids(conn, kairos_proto::METRIC_CPU_LOAD_5M)?,
            Vec::new(),
        ),
        _ => (
            series_ids(conn, crate::protocol::METRIC_CPU_USAGE)?,
            Vec::new(),
        ),
    };
    let mut without_metric = 0u64;
    let mut fires = 0u64;
    let mut quick = 0u64;
    let mut involved = 0u64;
    let mut per_day: BTreeMap<String, u64> = BTreeMap::new();
    let mut oldest: Option<i64> = None;
    let mut examples = Vec::new();
    let mut alt: BTreeMap<i64, u64> = BTreeMap::new();
    let candidates = [for_secs * 3 / 2, for_secs * 2, for_secs.max(900)];
    for (host_id, agent, name) in &hosts {
        let all = host_data(conn, table, *host_id, metric, p, from, &std_ids)?;
        if all.iter().all(|d| d.is_empty()) {
            // 這台主機沒有回報這個指標（舊版 Pulse、Windows 的負載、沒有資產的主機）
            without_metric += 1;
        }
        let mut host_fires = 0;
        for data in &all {
            if let Some((t, _)) = data.first() {
                oldest = Some(oldest.map_or(*t, |o: i64| o.min(*t)));
            }
            let r = replay(data, step, op, threshold, for_secs, clear, clear_for);
            host_fires += r.fires.len();
            for (at, dur) in &r.fires {
                fires += 1;
                if dur.is_some_and(|d| d <= 600) {
                    quick += 1;
                }
                let day = chrono::DateTime::from_timestamp(*at, 0)
                    .unwrap_or_default()
                    .with_timezone(&offset)
                    .format("%Y-%m-%d")
                    .to_string();
                *per_day.entry(day).or_default() += 1;
            }
            for c in candidates {
                if c > for_secs {
                    *alt.entry(c).or_default() +=
                        replay(data, step, op, threshold, c, clear, clear_for)
                            .fires
                            .len() as u64;
                }
            }
        }
        if host_fires > 0 {
            involved += 1;
            if examples.len() < 5 {
                examples.push(json!({ "agent_id": agent, "hostname": name, "fires": host_fires }));
            }
        }
    }
    let data_days = oldest.map(|o| ((now_ms / 1000 - o) as f64 / 86_400.0).min(f64::from(days)));
    let noisy = fires >= 5 && quick * 2 > fires;
    let suggestion = if noisy {
        alt.iter()
            .find(|(_, n)| **n * 2 <= fires)
            .map(|(c, n)| json!({ "for_secs": c, "fires": n }))
    } else {
        None
    };
    let mut notes = Vec::new();
    if step == 3600 {
        notes.push(format!(
            "範圍內有 {} 台主機，超過 {MAX_HOSTS_5M} 台，以 1 小時彙總試算",
            hosts.len()
        ));
    }
    if let Some(d) = data_days
        && d < f64::from(days) - 0.5
    {
        notes.push(format!("只有 {d:.0} 天的資料，預覽僅供參考"));
    }
    if data_days.is_none() {
        notes.push("範圍內沒有這個指標的彙總資料".to_string());
    }
    if without_metric > 0 && !hosts.is_empty() {
        notes.push(format!(
            "範圍內的 {} 台主機中，有 {without_metric} 台沒有回報這個指標（Pulse 版本較舊、Windows 沒有負載、或尚未收到資產），不會觸發",
            hosts.len()
        ));
    }
    if for_secs < step {
        notes.push(format!(
            "持續時間少於 {} 分鐘，以 {} 分鐘計",
            step / 60,
            step / 60
        ));
    }
    Ok(json!({
        "supported": true,
        "days": days,
        "data_days": data_days,
        "resolution_secs": step,
        "approximate": true,
        "hosts_in_scope": hosts.len(),
        "hosts_without_metric": without_metric,
        "fires": fires,
        "hosts_involved": involved,
        "self_recovered_within_10m": quick,
        "noise_ratio": if fires > 0 { Some(quick as f64 / fires as f64) } else { None },
        "noisy": noisy,
        "suggestion": suggestion,
        "per_day": per_day.into_iter().map(|(d, n)| json!({"date": d, "fires": n})).collect::<Vec<_>>(),
        "examples": examples,
        "notes": notes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(values: &[f64]) -> Series {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| (i as i64 * 300, *v))
            .collect()
    }

    #[test]
    fn replay_counts_sustained_conditions_with_hysteresis() {
        // 95 持續 3 個桶（15 分鐘）→ 持續 10 分鐘的規則觸發一次；恢復到 85（未低於恢復門檻 80）仍觸發中
        let d = data(&[50.0, 95.0, 95.0, 95.0, 85.0, 70.0, 70.0, 95.0, 95.0]);
        let r = replay(&d, 300, ">", 90.0, 600, Some(80.0), 0);
        assert_eq!(r.fires.len(), 2);
        assert_eq!(r.fires[0].0, 900); // 第 2 個桶結束時達到 10 分鐘
        assert_eq!(r.fires[0].1, Some(1800 - 900));
        assert_eq!(r.fires[1], (2700, None));
        // 只持續 5 分鐘：不觸發
        let r = replay(
            &data(&[95.0, 50.0, 95.0, 50.0]),
            300,
            ">",
            90.0,
            600,
            None,
            0,
        );
        assert!(r.fires.is_empty());
        // 缺資料中斷連續
        let gap: Series = vec![(0, 95.0), (900, 95.0)];
        assert!(replay(&gap, 300, ">", 90.0, 600, None, 0).fires.is_empty());
        // 小於：記憶體低於門檻
        let r = replay(&data(&[5.0, 5.0, 50.0]), 300, "<", 10.0, 300, None, 0);
        assert_eq!(r.fires.len(), 1);
        assert_eq!(r.fires[0].1, Some(600));
    }
}
