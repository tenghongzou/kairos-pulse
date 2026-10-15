//! 指標趨勢（`kairos/API.md` 第 5.3 節；設計包 §24 #5、S05 趨勢分頁）。
//!
//! 依範圍自動選原始資料、5 分鐘或 1 小時彙總（也可指定），回傳每個點的 min／max／avg／count；
//! 有 label 的序列分開；點數超過上限時把相鄰的點合併成較大的桶。彙總表中尚未寫出的最新部分
//! （主機的 `rollup_until` 之後）由原始資料即時計算。所有查詢在同一個讀取交易中（一致的快照）。

use chrono::{DateTime, Days, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use super::ApiContext;
use super::db::query_failed;
use super::hosts::parse_agent_id;
use super::http::{ApiError, Query, ts_ms};
use crate::hosts_cli::parse_duration;
use crate::protocol::{METRIC_CPU_USAGE, METRIC_MEMORY_TOTAL, METRIC_MEMORY_USED};
use crate::store::codec::{self, BYTES_PER_MB, FORMAT_LEGACY, FORMAT_METRICS};
use crate::store::derive::{DERIVED_ESTIMATED, DERIVED_RATE, derived_name, estimated_usage};
use crate::store::rollup::{Agg, BUCKET_1H_SECS, BUCKET_5M_SECS, bucket_start};
use kairos_proto::{
    CounterSample, METRIC_FILESYSTEM_AVAILABLE, METRIC_FILESYSTEM_TOTAL, METRIC_FILESYSTEM_USAGE,
    METRIC_UPTIME, MetricKind, counter_rate, standard_metric,
};

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
    /// 這個點（的一部分）是推算值（舊版 Pulse 的磁碟使用率；METRICS.md 第 9.2 節）
    est: bool,
}

/// 一個桶的（實際值, 推算值）
type BucketPair = (Option<Agg>, Option<Agg>);

/// 計數器的呈現方式（`transform` 參數）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    /// 每秒速率（計數器的預設）
    Rate,
    /// 原始累計值（只有原始資料解析度）
    None,
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
    transform: Transform,
}

/// 一條輸出的序列
#[derive(Debug)]
struct OutSeries {
    unit: String,
    labels: BTreeMap<String, String>,
    /// `rate`、`estimated` 或 `None`（原始序列）
    derived: Option<&'static str>,
    points: Vec<Point>,
}

/// 查詢的結果
#[derive(Debug)]
struct TrendResult {
    resolution: Resolution,
    bucket_secs: Option<i64>,
    /// `gauge` 或 `counter`
    kind: &'static str,
    series: Vec<OutSeries>,
    truncated: bool,
}

/// 輸出的一條序列由哪些資料組成
#[derive(Debug, Default)]
struct Group {
    unit: String,
    labels: String,
    derived: Option<&'static str>,
    /// 彙總中的序列：(series_id, 是否為推算值)。推算值只補沒有實際值的桶
    rollup: Vec<(i64, bool)>,
    /// 原始資料中直接取值的序列
    raw: Option<i64>,
    /// 原始資料中的計數器（以相鄰兩筆算出速率）
    counter: Option<i64>,
    /// 原始資料中推算使用率的 total、available 序列
    estimate: Option<(i64, i64)>,
}

/// 原始資料的一筆 report：取樣時間與（序列 → 值）
fn read_reports(
    conn: &Connection,
    host_id: i64,
    from_ms: i64,
    to_ms: i64,
    legacy: [Option<i64>; 3],
    mut f: impl FnMut(i64, &HashMap<i64, serde_json::Number>),
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT ts, format, body FROM reports WHERE host_id = ?1 AND ts >= ?2 AND ts < ?3 ORDER BY ts",
    )?;
    let mut rows = stmt.query(params![host_id, from_ms, to_ms])?;
    let mut values: HashMap<i64, serde_json::Number> = HashMap::new();
    while let Some(r) = rows.next()? {
        let t: i64 = r.get(0)?;
        let format: i64 = r.get(1)?;
        let rusqlite::types::ValueRef::Blob(body) = r.get_ref(2)? else {
            continue;
        };
        values.clear();
        match format {
            FORMAT_LEGACY => {
                if let Some((cpu, total, used)) = codec::decode_legacy(body) {
                    let nums = [
                        serde_json::Number::from_f64(f64::from(cpu)),
                        Some(serde_json::Number::from(total * BYTES_PER_MB)),
                        Some(serde_json::Number::from(used * BYTES_PER_MB)),
                    ];
                    for (id, v) in legacy.iter().zip(nums) {
                        if let (Some(id), Some(v)) = (id, v) {
                            values.insert(*id, v);
                        }
                    }
                }
            }
            FORMAT_METRICS => {
                values.extend(codec::decode_metrics(body).unwrap_or_default());
            }
            _ => continue,
        }
        f(t, &values);
    }
    Ok(())
}

/// 合併成 `bucket_secs` 的桶
fn regroup(points: &[Point], bucket_secs: i64) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::new();
    for p in points {
        let t = bucket_start(p.t, bucket_secs) * 1000;
        match out.last_mut() {
            Some(last) if last.t == t => {
                last.agg.merge(&p.agg);
                last.est |= p.est;
            }
            _ => out.push(Point {
                t,
                agg: p.agg,
                est: p.est,
            }),
        }
    }
    out
}

/// 這台主機這個名稱的序列：(series_id, unit, labels, derived)。v8 起以 `host_series` 只列出這台主機
/// 用過的序列；沒有對應時（例如 v7 升級時只出現在原始資料、尚未彙總的序列）改列出這個名稱的所有序列
fn host_series_of(
    conn: &Connection,
    host_id: i64,
    name: &str,
) -> rusqlite::Result<Vec<(i64, String, String, i64)>> {
    let collect = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> rusqlite::Result<Vec<_>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(p, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect()
    };
    let list = collect(
        "SELECT s.series_id, s.unit, s.labels, s.derived FROM series s
         JOIN host_series h USING (series_id) WHERE h.host_id = ?1 AND s.name = ?2",
        &[&host_id, &name],
    )?;
    if !list.is_empty() {
        return Ok(list);
    }
    collect(
        "SELECT series_id, unit, labels, derived FROM series WHERE name = ?1",
        &[&name],
    )
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
    let mut series = host_series_of(&tx, host_id, &q.name).map_err(&failed)?;
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
    series.retain(|(_, _, labels, _)| {
        let labels = parse_labels(labels);
        q.labels.iter().all(|(k, v)| labels.get(k) == Some(v))
    });
    let std = standard_metric(&q.name);
    let counter = std.is_some_and(|s| s.kind == MetricKind::Counter);
    let resolution_req = q.requested;
    if counter
        && q.transform == Transform::None
        && resolution_req.is_some_and(|r| r != Resolution::Raw)
    {
        return Err(ApiError::param(
            "transform",
            "transform=none（原始累計值）只有原始資料解析度，resolution 必須是 raw 或 auto",
        ));
    }
    // 輸出的序列（依單位與 label 合併原始、速率、推算）
    let mut groups: BTreeMap<(String, String), Group> = BTreeMap::new();
    for (id, unit, labels, derived) in &series {
        let is_std_unit = std.is_some_and(|s| s.unit == unit);
        match *derived {
            DERIVED_RATE => {
                if counter && q.transform == Transform::Rate {
                    let g = groups.entry((unit.clone(), labels.clone())).or_default();
                    g.unit.clone_from(unit);
                    g.labels.clone_from(labels);
                    g.derived = Some("rate");
                    g.rollup.push((*id, false));
                }
            }
            DERIVED_ESTIMATED => {
                let g = groups.entry((unit.clone(), labels.clone())).or_default();
                g.unit.clone_from(unit);
                g.labels.clone_from(labels);
                g.rollup.push((*id, true));
            }
            _ if counter && is_std_unit => {
                let (out_unit, rate) = match q.transform {
                    Transform::Rate => (format!("{unit}/s"), true),
                    Transform::None => (unit.clone(), false),
                };
                let g = groups
                    .entry((out_unit.clone(), labels.clone()))
                    .or_default();
                g.unit = out_unit;
                g.labels.clone_from(labels);
                if rate {
                    g.derived = Some("rate");
                    g.counter = Some(*id);
                } else {
                    g.raw = Some(*id);
                }
            }
            _ => {
                let g = groups.entry((unit.clone(), labels.clone())).or_default();
                g.unit.clone_from(unit);
                g.labels.clone_from(labels);
                g.raw = Some(*id);
                g.rollup.push((*id, false));
            }
        }
    }
    // 推算的使用率：原始資料中以同一組 label 的 total、available 計算（彙總中已有推算序列）
    if q.name == METRIC_FILESYSTEM_USAGE {
        let totals = host_series_of(&tx, host_id, METRIC_FILESYSTEM_TOTAL).map_err(&failed)?;
        let avails = host_series_of(&tx, host_id, METRIC_FILESYSTEM_AVAILABLE).map_err(&failed)?;
        for (tid, tunit, labels, tder) in &totals {
            if *tder != 0 || tunit != "By" {
                continue;
            }
            let lbl = parse_labels(labels);
            if !q.labels.iter().all(|(k, v)| lbl.get(k) == Some(v)) {
                continue;
            }
            if let Some((aid, ..)) = avails
                .iter()
                .find(|a| a.2 == *labels && a.3 == 0 && a.1 == "By")
            {
                let g = groups.entry(("%".to_string(), labels.clone())).or_default();
                g.unit = "%".to_string();
                g.labels.clone_from(labels);
                g.estimate = Some((*tid, *aid));
            }
        }
    }
    // 只有推算的資料（沒有 Pulse 送的使用率）的序列標示為推算
    for g in groups.values_mut() {
        let only_estimates = g.rollup.iter().all(|(_, e)| *e);
        if g.raw.is_none()
            && g.counter.is_none()
            && only_estimates
            && (!g.rollup.is_empty() || g.estimate.is_some())
        {
            g.derived = Some("estimated");
        }
    }
    let mut groups: Vec<Group> = groups.into_values().collect();
    let truncated = groups.len() > MAX_SERIES;
    groups.truncate(MAX_SERIES);
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
                    "SELECT series_id FROM series WHERE name = ?1 AND unit = ?2 AND labels = '' AND derived = 0",
                    params![name, unit],
                    |r| r.get(0),
                )
                .optional()
                .map_err(&failed)?;
        }
    }
    let uptime_id: Option<i64> = if groups.iter().any(|g| g.counter.is_some()) {
        tx.query_row(
            "SELECT series_id FROM series WHERE name = ?1 AND unit = 's' AND labels = '' AND derived = 0",
            [METRIC_UPTIME],
            |r| r.get(0),
        )
        .optional()
        .map_err(&failed)?
    } else {
        None
    };

    let resolution = if counter && q.transform == Transform::None {
        Resolution::Raw
    } else {
        resolution_req.unwrap_or_else(|| {
            choose_resolution(
                q.from_ms,
                q.to_ms,
                interval.unwrap_or(DEFAULT_REPORT_INTERVAL_MS),
                q.max_points,
                &q.starts,
            )
        })
    };

    // 原始資料：每條輸出序列在 [from, to) 的 (時間, 值, 是否推算)
    let read_raw_points = |from: i64, to: i64| -> rusqlite::Result<Vec<Vec<(i64, f64, bool)>>> {
        let mut out: Vec<Vec<(i64, f64, bool)>> = groups.iter().map(|_| Vec::new()).collect();
        // 計數器的上一筆：from 之前最近的一筆 report
        let mut prev: Vec<Option<CounterSample>> = groups.iter().map(|_| None).collect();
        let need_prev = groups.iter().any(|g| g.counter.is_some());
        let mut step = |t: i64, values: &HashMap<i64, serde_json::Number>, emit: bool| {
            let uptime = uptime_id
                .and_then(|u| values.get(&u))
                .and_then(|v| v.as_u64());
            for (i, g) in groups.iter().enumerate() {
                if let Some(id) = g.raw
                    && let Some(v) = values.get(&id).and_then(|v| v.as_f64())
                {
                    if emit {
                        out[i].push((t, v, false));
                    }
                    continue;
                }
                if let Some(c) = g.counter {
                    if let Some(v) = values.get(&c).and_then(|v| v.as_u64()) {
                        let cur = CounterSample {
                            value: v,
                            ts_ms: t,
                            uptime_secs: uptime,
                        };
                        if let Some(p) = prev[i]
                            && emit
                            && let Some(rate) = counter_rate(p, cur)
                        {
                            out[i].push((t, rate, false));
                        }
                        if prev[i].is_none_or(|p| t > p.ts_ms) {
                            prev[i] = Some(cur);
                        }
                    }
                    continue;
                }
                if emit
                    && let Some((tid, aid)) = g.estimate
                    && let (Some(total), Some(avail)) = (
                        values.get(&tid).and_then(|v| v.as_f64()),
                        values.get(&aid).and_then(|v| v.as_f64()),
                    )
                    && let Some(v) = estimated_usage(total, avail)
                {
                    out[i].push((t, v, true));
                }
            }
        };
        if need_prev {
            let seed: Option<i64> = tx
                .query_row(
                    "SELECT ts FROM reports WHERE host_id = ?1 AND ts < ?2 ORDER BY ts DESC LIMIT 1",
                    params![host_id, from],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(t) = seed {
                read_reports(&tx, host_id, t, t + 1, legacy, |t, v| step(t, v, false))?;
            }
        }
        read_reports(&tx, host_id, from, to, legacy, |t, v| step(t, v, true))?;
        Ok(out)
    };

    let mut points: Vec<Vec<Point>> = groups.iter().map(|_| Vec::new()).collect();
    match resolution {
        Resolution::Raw => {
            let raw = read_raw_points(q.from_ms, q.to_ms).map_err(&failed)?;
            for (i, list) in raw.into_iter().enumerate() {
                points[i] = list
                    .into_iter()
                    .map(|(t, v, est)| Point {
                        t,
                        agg: Agg::new(v),
                        est,
                    })
                    .collect();
            }
        }
        Resolution::M5 | Resolution::H1 => {
            let (table, secs) = match resolution {
                Resolution::M5 => ("rollup_5m", BUCKET_5M_SECS),
                _ => ("rollup_1h", BUCKET_1H_SECS),
            };
            // (group, bucket) → (實際值, 推算值)
            let mut buckets: Vec<BTreeMap<i64, BucketPair>> =
                groups.iter().map(|_| BTreeMap::new()).collect();
            let mut by_id: HashMap<i64, (usize, bool)> = HashMap::new();
            for (i, g) in groups.iter().enumerate() {
                for (id, est) in &g.rollup {
                    by_id.insert(*id, (i, *est));
                }
            }
            let put = |slot: &mut (Option<Agg>, Option<Agg>), agg: Agg, est: bool| {
                let target = if est { &mut slot.1 } else { &mut slot.0 };
                match target {
                    Some(a) => a.merge(&agg),
                    None => *target = Some(agg),
                }
            };
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
                    let Some(&(i, est)) = by_id.get(&id) else {
                        continue;
                    };
                    let agg = Agg {
                        min: r.get(2).map_err(&failed)?,
                        max: r.get(3).map_err(&failed)?,
                        sum: r.get(4).map_err(&failed)?,
                        count: u64::try_from(r.get::<_, i64>(5).map_err(&failed)?).unwrap_or(0),
                    };
                    let bucket: i64 = r.get(0).map_err(&failed)?;
                    put(buckets[i].entry(bucket).or_default(), agg, est);
                }
            }
            // 尚未寫出彙總的最新部分：由原始資料計算
            let tail_from = (from_bucket * 1000).max(rollup_until);
            if q.to_ms > tail_from {
                let raw = read_raw_points(tail_from, q.to_ms).map_err(&failed)?;
                for (i, list) in raw.into_iter().enumerate() {
                    for (t, v, est) in list {
                        put(
                            buckets[i].entry(bucket_start(t, secs)).or_default(),
                            Agg::new(v),
                            est,
                        );
                    }
                }
            }
            for (i, b) in buckets.into_iter().enumerate() {
                points[i] = b
                    .into_iter()
                    .filter_map(|(bucket, (real, est))| {
                        // Pulse 送的實際值優先；沒有時才用推算值
                        let (agg, is_est) = match (real, est) {
                            (Some(a), _) => (a, false),
                            (None, Some(e)) => (e, true),
                            (None, None) => return None,
                        };
                        Some(Point {
                            t: bucket * 1000,
                            agg,
                            est: is_est,
                        })
                    })
                    .collect();
            }
        }
    }
    drop(tx);

    // 點數上限：相鄰的點合併成較大的桶（所有序列用同一個桶長，時間對齊）
    let base = resolution.base_secs();
    let range_secs = ((q.to_ms - q.from_ms) / 1000).max(1);
    let too_many = points.iter().any(|p| p.len() > q.max_points);
    let bucket_secs = if too_many {
        let per = (range_secs + q.max_points as i64 - 2) / (q.max_points as i64 - 1).max(1);
        let b = ((per.max(base) + base - 1) / base) * base;
        for list in points.iter_mut() {
            *list = regroup(list, b);
        }
        Some(b)
    } else if resolution == Resolution::Raw {
        None
    } else {
        Some(base)
    };
    let series = groups
        .into_iter()
        .zip(points)
        .map(|(g, points)| OutSeries {
            labels: parse_labels(&g.labels),
            unit: g.unit,
            derived: g.derived,
            points,
        })
        .collect();
    Ok(Some(TrendResult {
        resolution,
        bucket_secs,
        kind: if counter { "counter" } else { "gauge" },
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

const TREND_PARAMS: [&str; 6] = [
    "range",
    "from",
    "to",
    "resolution",
    "max_points",
    "transform",
];

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
    let transform = match q.get("transform").unwrap_or("rate") {
        "rate" => Transform::Rate,
        "none" => Transform::None,
        other => {
            return Err(ApiError::param(
                "transform",
                format!("transform 的值 \"{other}\" 無效（rate、none）"),
            ));
        }
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
        transform,
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
            kind: match standard_metric(name) {
                Some(s) if s.kind == MetricKind::Counter => "counter",
                _ => "gauge",
            },
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
        .map(|s| {
            let mut v = json!({
                "unit": s.unit,
                "labels": s.labels,
                "kind": result.kind,
                "points": s.points.iter().map(|p| {
                    let mut o = json!({
                        "t": ts_ms(p.t),
                        "min": p.agg.min,
                        "max": p.agg.max,
                        "avg": if p.agg.count > 0 { p.agg.sum / p.agg.count as f64 } else { f64::NAN },
                        "count": p.agg.count,
                    });
                    if p.est {
                        o["estimated"] = json!(true);
                    }
                    o
                }).collect::<Vec<_>>(),
            });
            if let Some(d) = s.derived {
                v["derived"] = json!(d);
            }
            v
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
            "kind": result.kind,
            "transform": if result.kind == "counter" {
                Some(match transform { Transform::Rate => "rate", Transform::None => "none" })
            } else {
                None
            },
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
                        "SELECT series_id FROM series WHERE name = ?1 AND unit = ?2 AND labels = '' AND derived = 0",
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
    let mut out: Vec<(String, String, String, Option<&'static str>)> = Vec::new();
    let mut stmt = conn
        .prepare("SELECT name, unit, labels, derived FROM series WHERE series_id = ?1")
        .map_err(&failed)?;
    for id in ids {
        if let Some((name, unit, labels, derived)) = stmt
            .query_row([id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .optional()
            .map_err(&failed)?
        {
            match derived {
                // 彙總中的速率序列代表原本的計數器（v8 起計數器只彙總速率）
                DERIVED_RATE => {
                    let unit = unit.strip_suffix("/s").unwrap_or(&unit).to_string();
                    out.push((name, unit, labels, None));
                }
                d => out.push((name, unit, labels, derived_name(d))),
            }
        }
    }
    // 推算的使用率：有 total 與 available、沒有 usage 的掛載點（0.2.0～0.7.0 的 Pulse）
    let has = |out: &[(String, String, String, Option<&'static str>)], name: &str, labels: &str| {
        out.iter().any(|o| o.0 == name && o.2 == labels)
    };
    let fs: Vec<String> = out
        .iter()
        .filter(|o| o.0 == METRIC_FILESYSTEM_TOTAL && o.3.is_none())
        .map(|o| o.2.clone())
        .collect();
    for labels in fs {
        if has(&out, METRIC_FILESYSTEM_AVAILABLE, &labels)
            && !has(&out, METRIC_FILESYSTEM_USAGE, &labels)
        {
            out.push((
                METRIC_FILESYSTEM_USAGE.to_string(),
                "%".to_string(),
                labels,
                Some("estimated"),
            ));
        }
    }
    out.sort();
    out.dedup();
    let metrics = out
        .into_iter()
        .map(|(name, unit, labels, derived)| {
            let labels: BTreeMap<String, String> = if labels.is_empty() {
                BTreeMap::new()
            } else {
                serde_json::from_str(&labels).unwrap_or_default()
            };
            let std = standard_metric(&name).filter(|s| s.unit == unit);
            let kind = match std {
                Some(s) if s.kind == MetricKind::Counter => "counter",
                _ => "gauge",
            };
            let mut v = json!({
                "name": name,
                "unit": unit,
                "labels": labels,
                "kind": kind,
                "standard": std.is_some(),
            });
            if let Some(d) = derived {
                v["derived"] = json!(d);
            }
            v
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
            est: false,
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
