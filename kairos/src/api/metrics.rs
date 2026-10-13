//! 指標趨勢（`kairos/API.md` 第 5.3 節；設計包 §24 #5、S05 趨勢分頁）。
//!
//! 依範圍自動選原始資料、5 分鐘或 1 小時彙總（也可指定），回傳每個點的 min／max／avg／count；
//! 有 label 的序列分開；點數超過上限時把相鄰的點合併成較大的桶。彙總表中尚未寫出的最新部分
//! （主機的 `rollup_until` 之後）由原始資料即時計算。所有查詢在同一個讀取交易中（一致的快照）。

use chrono::{DateTime, Days, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use super::ApiContext;
use super::db::query_failed;
use super::hosts::parse_agent_id;
use super::http::{ApiError, Query, ts_ms};
use crate::hosts_cli::parse_duration;
use crate::protocol::{METRIC_CPU_USAGE, METRIC_MEMORY_TOTAL, METRIC_MEMORY_USED};
use crate::store::codec::{self, BYTES_PER_MB, FORMAT_LEGACY, FORMAT_METRICS};
use crate::store::rollup::{Agg, BUCKET_1H_SECS, BUCKET_5M_SECS, bucket_start};

/// 每條序列的點數上限：預設、最小、最大
pub const DEFAULT_MAX_POINTS: usize = 1000;
pub const MIN_MAX_POINTS: usize = 10;
pub const MAX_MAX_POINTS: usize = 5000;
/// 序列數上限（有 label 的指標，例如每個掛載點一條）
pub const MAX_SERIES: usize = 64;
/// 查詢範圍的上限
pub const MAX_RANGE: Duration = Duration::from_secs(3660 * 86_400);
/// 沒有指定範圍時
pub const DEFAULT_RANGE: Duration = Duration::from_secs(3600);
/// 原始資料的點數以這個 report 間隔估算（主機沒有宣告時；Pulse 的預設）
pub const DEFAULT_REPORT_INTERVAL_MS: i64 = 5000;

/// 解析度
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Raw,
    M5,
    H1,
}

impl Resolution {
    pub fn as_str(self) -> &'static str {
        match self {
            Resolution::Raw => "raw",
            Resolution::M5 => "5m",
            Resolution::H1 => "1h",
        }
    }

    /// 桶的長度（秒）；原始資料為 1（以秒合併）
    fn base_secs(self) -> i64 {
        match self {
            Resolution::Raw => 1,
            Resolution::M5 => BUCKET_5M_SECS,
            Resolution::H1 => BUCKET_1H_SECS,
        }
    }
}

/// 各層保留期限的起點（Unix 毫秒；與保留清除相同的「今天與前 N 天」，0 天為不限）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionStarts {
    pub raw: Option<i64>,
    pub m5: Option<i64>,
    pub h1: Option<i64>,
}

impl RetentionStarts {
    pub fn new(now: DateTime<Utc>, raw: u32, m5: u32, h1: u32) -> Self {
        let start = |days: u32| -> Option<i64> {
            if days == 0 {
                return None;
            }
            let day = now
                .date_naive()
                .checked_sub_days(Days::new(u64::from(days)))?;
            Some(day.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis())
        };
        Self {
            raw: start(raw),
            m5: start(m5),
            h1: start(h1),
        }
    }

    fn of(&self, r: Resolution) -> Option<i64> {
        match r {
            Resolution::Raw => self.raw,
            Resolution::M5 => self.m5,
            Resolution::H1 => self.h1,
        }
    }
}

/// `auto` 的選擇：保留期限涵蓋 `from`、且預估點數不超過上限的最細解析度；都不滿足時 1 小時
pub fn choose_resolution(
    from_ms: i64,
    to_ms: i64,
    interval_ms: i64,
    max_points: usize,
    starts: &RetentionStarts,
) -> Resolution {
    let range_ms = (to_ms - from_ms).max(0);
    let est = |step_ms: i64| (range_ms / step_ms.max(1)) as usize;
    for (r, step) in [
        (Resolution::Raw, interval_ms.max(1000)),
        (Resolution::M5, BUCKET_5M_SECS * 1000),
    ] {
        if starts.of(r).is_none_or(|s| from_ms >= s) && est(step) <= max_points {
            return r;
        }
    }
    Resolution::H1
}

/// 一個點（桶）
#[derive(Debug, Clone, Copy, PartialEq)]
struct Point {
    /// 桶的開始（Unix 毫秒；原始資料為取樣時間）
    t: i64,
    agg: Agg,
}

/// 查詢的參數
#[derive(Debug, Clone)]
struct TrendQuery {
    agent_id: String,
    name: String,
    from_ms: i64,
    to_ms: i64,
    requested: Option<Resolution>,
    max_points: usize,
    labels: BTreeMap<String, String>,
    starts: RetentionStarts,
}

/// 查詢的結果
#[derive(Debug)]
struct TrendResult {
    resolution: Resolution,
    bucket_secs: Option<i64>,
    series: Vec<(String, BTreeMap<String, String>, Vec<Point>)>,
    truncated: bool,
}

/// 從原始資料讀出 `wanted` 序列在 [from, to) 的值，交給 `f(series_id, ts, value)`
fn read_raw(
    conn: &Connection,
    host_id: i64,
    from_ms: i64,
    to_ms: i64,
    wanted: &HashSet<i64>,
    legacy: [Option<i64>; 3],
    mut f: impl FnMut(i64, i64, f64),
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT ts, format, body FROM reports WHERE host_id = ?1 AND ts >= ?2 AND ts < ?3 ORDER BY ts",
    )?;
    let mut rows = stmt.query(params![host_id, from_ms, to_ms])?;
    while let Some(r) = rows.next()? {
        let t: i64 = r.get(0)?;
        let format: i64 = r.get(1)?;
        let rusqlite::types::ValueRef::Blob(body) = r.get_ref(2)? else {
            continue;
        };
        match format {
            FORMAT_LEGACY => {
                if let Some((cpu, total, used)) = codec::decode_legacy(body) {
                    let values = [
                        f64::from(cpu),
                        (total * BYTES_PER_MB) as f64,
                        (used * BYTES_PER_MB) as f64,
                    ];
                    for (id, v) in legacy.iter().zip(values) {
                        if let Some(id) = id
                            && wanted.contains(id)
                        {
                            f(*id, t, v);
                        }
                    }
                }
            }
            FORMAT_METRICS => {
                for (id, v) in codec::decode_metrics(body).unwrap_or_default() {
                    if wanted.contains(&id)
                        && let Some(v) = v.as_f64()
                    {
                        f(id, t, v);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// 合併成 `bucket_secs` 的桶
fn regroup(points: &[Point], bucket_secs: i64) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::new();
    for p in points {
        let t = bucket_start(p.t, bucket_secs) * 1000;
        match out.last_mut() {
            Some(last) if last.t == t => last.agg.merge(&p.agg),
            _ => out.push(Point { t, agg: p.agg }),
        }
    }
    out
}

fn run_query(conn: &Connection, q: &TrendQuery) -> Result<Option<TrendResult>, ApiError> {
    let failed = query_failed("指標趨勢");
    // 一致的快照：rollup_until、彙總與原始資料在同一個讀取交易中
    let tx = conn.unchecked_transaction().map_err(&failed)?;
    let Some((host_id, rollup_until, interval)) = tx
        .query_row(
            "SELECT host_id, rollup_until, report_interval_ms FROM hosts WHERE agent_id = ?1",
            [&q.agent_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(&failed)?
    else {
        return Ok(None);
    };
    // 這個名稱的所有序列（單位＋label 的組合）
    let mut series: Vec<(i64, String, String)> = {
        let mut stmt = tx
            .prepare("SELECT series_id, unit, labels FROM series WHERE name = ?1")
            .map_err(&failed)?;
        let rows = stmt
            .query_map([&q.name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(&failed)?;
        rows.collect::<rusqlite::Result<_>>().map_err(&failed)?
    };
    if series.is_empty() {
        return Err(ApiError::new(
            hyper::StatusCode::NOT_FOUND,
            "metric_not_found",
            format!("Kairos 沒有收過名為 {} 的指標", q.name),
        ));
    }
    let parse_labels = |s: &str| -> BTreeMap<String, String> {
        if s.is_empty() {
            BTreeMap::new()
        } else {
            serde_json::from_str(s).unwrap_or_default()
        }
    };
    series.retain(|(_, _, labels)| {
        let labels = parse_labels(labels);
        q.labels.iter().all(|(k, v)| labels.get(k) == Some(v))
    });
    series.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.1.cmp(&b.1)));
    let truncated = series.len() > MAX_SERIES;
    series.truncate(MAX_SERIES);
    let wanted: HashSet<i64> = series.iter().map(|s| s.0).collect();
    // v1 的三欄位 report 對應的標準序列
    let mut legacy = [None; 3];
    for (slot, (name, unit)) in legacy.iter_mut().zip([
        (METRIC_CPU_USAGE, "%"),
        (METRIC_MEMORY_TOTAL, "By"),
        (METRIC_MEMORY_USED, "By"),
    ]) {
        if name == q.name {
            *slot = tx
                .query_row(
                    "SELECT series_id FROM series WHERE name = ?1 AND unit = ?2 AND labels = ''",
                    params![name, unit],
                    |r| r.get(0),
                )
                .optional()
                .map_err(&failed)?;
        }
    }

    let resolution = q.requested.unwrap_or_else(|| {
        choose_resolution(
            q.from_ms,
            q.to_ms,
            interval.unwrap_or(DEFAULT_REPORT_INTERVAL_MS),
            q.max_points,
            &q.starts,
        )
    });
    let mut points: HashMap<i64, Vec<Point>> = HashMap::new();
    match resolution {
        Resolution::Raw => {
            read_raw(
                &tx,
                host_id,
                q.from_ms,
                q.to_ms,
                &wanted,
                legacy,
                |id, t, v| {
                    points.entry(id).or_default().push(Point {
                        t,
                        agg: Agg::new(v),
                    });
                },
            )
            .map_err(&failed)?;
        }
        Resolution::M5 | Resolution::H1 => {
            let (table, secs) = match resolution {
                Resolution::M5 => ("rollup_5m", BUCKET_5M_SECS),
                _ => ("rollup_1h", BUCKET_1H_SECS),
            };
            let mut buckets: HashMap<(i64, i64), Agg> = HashMap::new();
            let from_bucket = bucket_start(q.from_ms, secs);
            {
                let mut stmt = tx
                    .prepare(&format!(
                        "SELECT bucket, series_id, vmin, vmax, vsum, cnt FROM {table}
                         WHERE host_id = ?1 AND bucket >= ?2 AND bucket < ?3"
                    ))
                    .map_err(&failed)?;
                let mut rows = stmt
                    .query(params![host_id, from_bucket, q.to_ms.div_euclid(1000)])
                    .map_err(&failed)?;
                while let Some(r) = rows.next().map_err(&failed)? {
                    let id: i64 = r.get(1).map_err(&failed)?;
                    if !wanted.contains(&id) {
                        continue;
                    }
                    let agg = Agg {
                        min: r.get(2).map_err(&failed)?,
                        max: r.get(3).map_err(&failed)?,
                        sum: r.get(4).map_err(&failed)?,
                        count: u64::try_from(r.get::<_, i64>(5).map_err(&failed)?).unwrap_or(0),
                    };
                    let bucket: i64 = r.get(0).map_err(&failed)?;
                    buckets
                        .entry((id, bucket))
                        .and_modify(|a| a.merge(&agg))
                        .or_insert(agg);
                }
            }
            // 尚未寫出彙總的最新部分：由原始資料計算
            let tail_from = (from_bucket * 1000).max(rollup_until);
            if q.to_ms > tail_from {
                read_raw(
                    &tx,
                    host_id,
                    tail_from,
                    q.to_ms,
                    &wanted,
                    legacy,
                    |id, t, v| {
                        buckets
                            .entry((id, bucket_start(t, secs)))
                            .and_modify(|a| a.add(v))
                            .or_insert(Agg::new(v));
                    },
                )
                .map_err(&failed)?;
            }
            for ((id, bucket), agg) in buckets {
                points.entry(id).or_default().push(Point {
                    t: bucket * 1000,
                    agg,
                });
            }
            for list in points.values_mut() {
                list.sort_by_key(|p| p.t);
            }
        }
    }
    drop(tx);

    // 點數上限：相鄰的點合併成較大的桶（所有序列用同一個桶長，時間對齊）
    let base = resolution.base_secs();
    let range_secs = ((q.to_ms - q.from_ms) / 1000).max(1);
    let too_many = points.values().any(|p| p.len() > q.max_points);
    let bucket_secs = if too_many {
        let per = (range_secs + q.max_points as i64 - 2) / (q.max_points as i64 - 1).max(1);
        let b = ((per.max(base) + base - 1) / base) * base;
        for list in points.values_mut() {
            *list = regroup(list, b);
        }
        Some(b)
    } else if resolution == Resolution::Raw {
        None
    } else {
        Some(base)
    };
    let series = series
        .into_iter()
        .map(|(id, unit, labels): (i64, String, String)| {
            let labels = parse_labels(labels.as_str());
            (unit, labels, points.remove(&id).unwrap_or_default())
        })
        .collect();
    Ok(Some(TrendResult {
        resolution,
        bucket_secs,
        series,
        truncated,
    }))
}

fn parse_time(param: &str, text: &str) -> Result<DateTime<Utc>, ApiError> {
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| {
            ApiError::param(
                param,
                format!("{param} 的值 \"{text}\" 不是 RFC 3339 時間（例如 2026-10-01T00:00:00Z）"),
            )
        })
}

const TREND_PARAMS: [&str; 5] = ["range", "from", "to", "resolution", "max_points"];

/// `GET /api/v1/hosts/{agent_id}/metrics/{name}`
pub async fn trend(
    ctx: &Arc<ApiContext>,
    agent_id: &str,
    name: &str,
    q: &Query,
    now: DateTime<Utc>,
) -> Result<hyper::Response<super::http::ApiBody>, ApiError> {
    q.reject_unknown(&TREND_PARAMS, &["label."])?;
    let agent_id = parse_agent_id(agent_id)?;
    if !kairos_proto::is_valid_metric_name(name) {
        return Err(ApiError::param(
            "name",
            format!("\"{name}\" 不是合法的指標名稱（小寫、以 . 分段，例如 system.cpu.usage）"),
        ));
    }
    let (from, to) = match (q.get("range"), q.get("from"), q.get("to")) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            return Err(ApiError::param("range", "range 與 from／to 只能擇一"));
        }
        (Some(r), None, None) => {
            let d = parse_duration(r).filter(|d| !d.is_zero()).ok_or_else(|| {
                ApiError::param(
                    "range",
                    format!("range 的值 \"{r}\" 無效，例如 1h、24h、7d、30d"),
                )
            })?;
            let d = chrono::Duration::from_std(d.min(MAX_RANGE)).unwrap_or_default();
            (now - d, now)
        }
        (None, from, to) => {
            let to = to.map(|t| parse_time("to", t)).transpose()?.unwrap_or(now);
            let from = match from {
                Some(f) => parse_time("from", f)?,
                None => to - chrono::Duration::from_std(DEFAULT_RANGE).unwrap_or_default(),
            };
            (from, to)
        }
    };
    if from >= to {
        return Err(ApiError::param("from", "from 必須早於 to"));
    }
    if (to - from).to_std().unwrap_or_default() > MAX_RANGE {
        return Err(ApiError::param("from", "查詢範圍最長 3660 天"));
    }
    let requested = match q.get("resolution").unwrap_or("auto") {
        "auto" => None,
        "raw" => Some(Resolution::Raw),
        "5m" => Some(Resolution::M5),
        "1h" => Some(Resolution::H1),
        other => {
            return Err(ApiError::param(
                "resolution",
                format!("resolution 的值 \"{other}\" 無效（auto、raw、5m、1h）"),
            ));
        }
    };
    let max_points = match q.get("max_points") {
        None => DEFAULT_MAX_POINTS,
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|n| (MIN_MAX_POINTS..=MAX_MAX_POINTS).contains(n))
            .ok_or_else(|| {
                ApiError::param(
                    "max_points",
                    format!("max_points 必須介於 {MIN_MAX_POINTS}～{MAX_MAX_POINTS}"),
                )
            })?,
    };
    let labels: BTreeMap<String, String> = q
        .with_prefix("label.")
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let s = &ctx.config.storage;
    let starts = RetentionStarts::new(
        now,
        s.raw_retention_days,
        s.rollup_5m_retention_days,
        s.rollup_1h_retention_days,
    );
    let mut notes: Vec<String> = Vec::new();
    let mut from_ms = from.timestamp_millis();
    if let Some(h1) = starts.h1
        && from_ms < h1
    {
        from_ms = h1;
        notes.push(format!(
            "只保留最近 {} 天的 1 小時彙總（storage.rollup_1h_retention_days），查詢範圍的起點調整為 {}",
            s.rollup_1h_retention_days,
            ts_ms(h1)
        ));
        if from_ms >= to.timestamp_millis() {
            return Err(ApiError::param("from", "查詢範圍已超過所有資料的保留期限"));
        }
    }
    let tq = TrendQuery {
        agent_id: agent_id.clone(),
        name: name.to_string(),
        from_ms,
        to_ms: to.timestamp_millis(),
        requested,
        max_points,
        labels,
        starts,
    };
    let query = tq.clone();
    let result = match ctx
        .pool
        .read("指標趨勢", move |conn| run_query(conn, &query))
        .await?
    {
        Some(r) => r,
        // 剛上線、第一筆 report 還沒寫入清冊：沒有資料（不是 404）
        None if ctx.registry.live_get(&agent_id).is_some() => TrendResult {
            resolution: requested.unwrap_or(Resolution::Raw),
            bucket_secs: None,
            series: Vec::new(),
            truncated: false,
        },
        None => return Err(ApiError::host_not_found(&agent_id)),
    };
    if let Some(start) = starts.of(result.resolution)
        && tq.from_ms < start
    {
        let (what, key, days) = match result.resolution {
            Resolution::Raw => (
                "原始資料",
                "storage.raw_retention_days",
                s.raw_retention_days,
            ),
            Resolution::M5 => (
                "5 分鐘彙總",
                "storage.rollup_5m_retention_days",
                s.rollup_5m_retention_days,
            ),
            Resolution::H1 => (
                "1 小時彙總",
                "storage.rollup_1h_retention_days",
                s.rollup_1h_retention_days,
            ),
        };
        notes.push(format!(
            "{what}只保留 {days} 天（{key}），{} 之前沒有這個解析度的資料",
            ts_ms(start)
        ));
    }
    let series: Vec<Value> = result
        .series
        .iter()
        .map(|(unit, labels, points)| {
            json!({
                "unit": unit,
                "labels": labels,
                "points": points.iter().map(|p| json!({
                    "t": ts_ms(p.t),
                    "min": p.agg.min,
                    "max": p.agg.max,
                    "avg": if p.agg.count > 0 { p.agg.sum / p.agg.count as f64 } else { f64::NAN },
                    "count": p.agg.count,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(super::http::ok(
        now,
        json!({
            "agent_id": agent_id,
            "name": name,
            "from": ts_ms(tq.from_ms),
            "to": ts_ms(tq.to_ms),
            "resolution": result.resolution.as_str(),
            "requested_resolution": requested.map_or("auto", |r| r.as_str()),
            "bucket_secs": result.bucket_secs,
            "series": series,
            "truncated": result.truncated,
            "retention_days": {
                "raw": s.raw_retention_days,
                "rollup_5m": s.rollup_5m_retention_days,
                "rollup_1h": s.rollup_1h_retention_days,
            },
            "notes": notes,
        }),
    ))
}

/// 一台主機目前回報的指標與其時間（最近一次原始 report 的取樣時間）
type Available = (Vec<Value>, Option<i64>);

/// 這台主機目前回報的指標：最近一次原始 report 中的序列；沒有原始資料時取最近 7 天的 1 小時彙總
fn available(
    conn: &Connection,
    agent_id: &str,
    now_ms: i64,
) -> Result<Option<Available>, ApiError> {
    let failed = query_failed("指標清單");
    let Some(host_id) = conn
        .query_row(
            "SELECT host_id FROM hosts WHERE agent_id = ?1",
            [agent_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(&failed)?
    else {
        return Ok(None);
    };
    let latest: Option<(i64, i64, Vec<u8>)> = conn
        .query_row(
            "SELECT ts, format, body FROM reports WHERE host_id = ?1 ORDER BY ts DESC LIMIT 1",
            [host_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(&failed)?;
    let (as_of, ids): (Option<i64>, Vec<i64>) = match latest {
        Some((t, FORMAT_LEGACY, _)) => {
            let mut ids = Vec::new();
            for (name, unit) in [
                (METRIC_CPU_USAGE, "%"),
                (METRIC_MEMORY_TOTAL, "By"),
                (METRIC_MEMORY_USED, "By"),
            ] {
                if let Some(id) = conn
                    .query_row(
                        "SELECT series_id FROM series WHERE name = ?1 AND unit = ?2 AND labels = ''",
                        params![name, unit],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(&failed)?
                {
                    ids.push(id);
                }
            }
            (Some(t), ids)
        }
        Some((t, _, body)) => (
            Some(t),
            codec::decode_metrics(&body)
                .unwrap_or_default()
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
        ),
        None => {
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT series_id FROM rollup_1h WHERE host_id = ?1 AND bucket >= ?2",
                )
                .map_err(&failed)?;
            let rows = stmt
                .query_map(params![host_id, now_ms / 1000 - 7 * 86_400], |r| r.get(0))
                .map_err(&failed)?;
            (
                None,
                rows.collect::<rusqlite::Result<_>>().map_err(&failed)?,
            )
        }
    };
    let mut out: Vec<(String, String, String)> = Vec::new();
    let mut stmt = conn
        .prepare("SELECT name, unit, labels FROM series WHERE series_id = ?1")
        .map_err(&failed)?;
    for id in ids {
        if let Some(row) = stmt
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()
            .map_err(&failed)?
        {
            out.push(row);
        }
    }
    out.sort();
    out.dedup();
    let metrics = out
        .into_iter()
        .map(|(name, unit, labels)| {
            let labels: BTreeMap<String, String> = if labels.is_empty() {
                BTreeMap::new()
            } else {
                serde_json::from_str(&labels).unwrap_or_default()
            };
            json!({ "name": name, "unit": unit, "labels": labels })
        })
        .collect();
    Ok(Some((metrics, as_of)))
}

/// `GET /api/v1/hosts/{agent_id}/metrics`
pub async fn list(
    ctx: &Arc<ApiContext>,
    agent_id: &str,
    now: DateTime<Utc>,
) -> Result<hyper::Response<super::http::ApiBody>, ApiError> {
    let agent_id = parse_agent_id(agent_id)?;
    let id = agent_id.clone();
    let now_ms = now.timestamp_millis();
    let (metrics, as_of) = match ctx
        .pool
        .read("指標清單", move |conn| available(conn, &id, now_ms))
        .await?
    {
        Some(r) => r,
        None if ctx.registry.live_get(&agent_id).is_some() => (Vec::new(), None),
        None => return Err(ApiError::host_not_found(&agent_id)),
    };
    Ok(super::http::ok(
        now,
        json!({ "agent_id": agent_id, "as_of": as_of.map(ts_ms), "metrics": metrics }),
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn auto_resolution_follows_range_and_retention() {
        let now = DateTime::parse_from_rfc3339("2026-10-13T06:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let starts = RetentionStarts::new(now, 7, 31, 400);
        assert_eq!(
            starts.raw,
            Some(
                DateTime::parse_from_rfc3339("2026-10-06T00:00:00Z")
                    .unwrap()
                    .timestamp_millis()
            )
        );
        let to = now.timestamp_millis();
        let h = 3_600_000;
        let pick = |range_ms: i64| choose_resolution(to - range_ms, to, 5000, 1000, &starts);
        assert_eq!(pick(h), Resolution::Raw);
        assert_eq!(pick(24 * h), Resolution::M5);
        assert_eq!(pick(7 * 24 * h), Resolution::H1);
        assert_eq!(pick(30 * 24 * h), Resolution::H1);
        // 宣告較長的 report 間隔：原始資料可涵蓋更長的範圍
        assert_eq!(
            choose_resolution(to - 24 * h, to, 120_000, 1000, &starts),
            Resolution::Raw
        );
        // 超過原始資料的保留期限：即使點數不多也改用彙總
        let old = to - 10 * 24 * h;
        assert_eq!(
            choose_resolution(old, old + h, 5000, 1000, &starts),
            Resolution::M5
        );
        // 不依時間刪除（0 天）
        let forever = RetentionStarts::new(now, 0, 0, 0);
        assert_eq!(forever.raw, None);
        assert_eq!(
            choose_resolution(old, old + h, 5000, 1000, &forever),
            Resolution::Raw
        );
    }

    #[test]
    fn regrouping_keeps_min_max_sum_and_count() {
        let p = |t: i64, v: f64| Point {
            t,
            agg: Agg::new(v),
        };
        let points = vec![p(0, 1.0), p(400, 5.0), p(1000, 3.0), p(70_000, 2.0)];
        let out = regroup(&points, 60);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].t, 0);
        assert_eq!(out[0].agg.min, 1.0);
        assert_eq!(out[0].agg.max, 5.0);
        assert_eq!(out[0].agg.sum, 9.0);
        assert_eq!(out[0].agg.count, 3);
        assert_eq!(out[1].t, 60_000);
    }
}
