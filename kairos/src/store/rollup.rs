//! 彙總（rollup）：每 5 分鐘與每小時的 min／max／sum／count。
//!
//! 寫入 report 的交易成功後，把每個數值加進記憶體中「尚未寫出」的 5 分鐘桶；桶結束
//! [`FLUSH_GRACE_SECS`] 秒後寫出到 `rollup_5m`，同一個交易中再合併進 `rollup_1h`，並把該主機的
//! `hosts.rollup_until` 推進到桶的結束時間。重新啟動時，各主機從 `rollup_until` 起重新讀取原始資料
//! 補回記憶體中的桶，因此不會漏算，也不會重複計算（見 `kairos/STORAGE.md`「彙總」）。

use std::collections::BTreeMap;

/// 5 分鐘桶的長度（秒）
pub const BUCKET_5M_SECS: i64 = 300;
/// 1 小時桶的長度（秒）
pub const BUCKET_1H_SECS: i64 = 3600;
/// 桶結束後再等這麼久才寫出：report 的時間是讀到該行的時間，排隊與限速可能讓它晚幾秒才進入 writer
pub const FLUSH_GRACE_SECS: i64 = 15;

/// 一個桶的統計值
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Agg {
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    pub count: u64,
}

impl Agg {
    pub fn new(v: f64) -> Self {
        Self {
            min: v,
            max: v,
            sum: v,
            count: 1,
        }
    }

    pub fn add(&mut self, v: f64) {
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.sum += v;
        self.count += 1;
    }
}

/// 桶的開始時間（Unix 秒）
pub fn bucket_start(ts_ms: i64, bucket_secs: i64) -> i64 {
    ts_ms.div_euclid(1000).div_euclid(bucket_secs) * bucket_secs
}

/// 記憶體中尚未寫出的 5 分鐘桶。key 為 (桶開始的 Unix 秒, host_id, series_id)，
/// 依桶、主機的順序寫出（見 [`Rollup::take_ready`]）
#[derive(Debug, Default)]
pub struct Rollup {
    open: BTreeMap<(i64, i64, i64), Agg>,
}

/// 寫出的一列
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RollupRow {
    pub bucket: i64,
    pub host_id: i64,
    pub series_id: i64,
    pub agg: Agg,
}

impl Rollup {
    /// 加入一個數值（`ts_ms` 為伺服器收到的時間）
    pub fn add(&mut self, host_id: i64, series_id: i64, ts_ms: i64, value: f64) {
        if !value.is_finite() {
            return;
        }
        let key = (bucket_start(ts_ms, BUCKET_5M_SECS), host_id, series_id);
        self.open
            .entry(key)
            .and_modify(|a| a.add(value))
            .or_insert_with(|| Agg::new(value));
    }

    /// 尚未寫出的列數
    pub fn len(&self) -> usize {
        self.open.len()
    }

    /// 是否有已結束（`now_ms` 已超過結束時間加上寬限）的桶
    pub fn has_ready(&self, now_ms: i64) -> bool {
        self.open
            .first_key_value()
            .is_some_and(|(&(bucket, _, _), _)| is_ready(bucket, now_ms))
    }

    /// 取出已結束的桶中最多約 `limit` 列；只在主機的邊界切開（同一主機同一個桶的列一定在同一批），
    /// 寫出該批時才能把這些主機的 `rollup_until` 推進到桶的結束時間
    pub fn take_ready(&mut self, now_ms: i64, limit: usize) -> Vec<RollupRow> {
        let mut out: Vec<RollupRow> = Vec::new();
        while let Some((&(bucket, host_id, series_id), _)) = self.open.first_key_value() {
            if !is_ready(bucket, now_ms) {
                break;
            }
            if out.len() >= limit
                && out
                    .last()
                    .is_some_and(|last| (last.bucket, last.host_id) != (bucket, host_id))
            {
                break;
            }
            let Some((_, agg)) = self.open.pop_first() else {
                break;
            };
            out.push(RollupRow {
                bucket,
                host_id,
                series_id,
                agg,
            });
        }
        out
    }

    /// 寫出失敗時放回（與之後加入的值合併）
    pub fn put_back(&mut self, rows: Vec<RollupRow>) {
        for row in rows {
            self.open
                .entry((row.bucket, row.host_id, row.series_id))
                .and_modify(|a| {
                    a.min = a.min.min(row.agg.min);
                    a.max = a.max.max(row.agg.max);
                    a.sum += row.agg.sum;
                    a.count += row.agg.count;
                })
                .or_insert(row.agg);
        }
    }

    /// 清空（資料庫被換掉時，host_id 與 series_id 都不再有效）
    pub fn clear(&mut self) {
        self.open.clear();
    }
}

fn is_ready(bucket: i64, now_ms: i64) -> bool {
    now_ms.div_euclid(1000) >= bucket + BUCKET_5M_SECS + FLUSH_GRACE_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_791_000_000_000; // 5 分鐘的整數倍附近的毫秒

    #[test]
    fn buckets_align_to_utc() {
        assert_eq!(bucket_start(0, 300), 0);
        assert_eq!(bucket_start(299_999, 300), 0);
        assert_eq!(bucket_start(300_000, 300), 300);
        assert_eq!(bucket_start(3_599_999, 3600), 0);
        assert_eq!(bucket_start(-1, 300), -300);
    }

    #[test]
    fn values_are_aggregated_per_bucket_host_and_series() {
        let mut r = Rollup::default();
        let b = bucket_start(T0, 300) * 1000;
        r.add(1, 1, b, 10.0);
        r.add(1, 1, b + 1000, 30.0);
        r.add(1, 1, b + 299_999, 20.0);
        r.add(1, 2, b, 5.0);
        r.add(2, 1, b, 7.0);
        r.add(1, 1, b + 300_000, 1.0); // 下一個桶
        r.add(1, 1, b, f64::NAN); // 非有限值不計
        assert_eq!(r.len(), 4);

        // 結束 + 寬限之前不寫出
        let end = b + 300_000;
        assert!(!r.has_ready(end + FLUSH_GRACE_SECS * 1000 - 1));
        assert!(r.take_ready(end, 100).is_empty());
        assert!(r.has_ready(end + FLUSH_GRACE_SECS * 1000));
        let rows = r.take_ready(end + FLUSH_GRACE_SECS * 1000, 100);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0].agg,
            Agg {
                min: 10.0,
                max: 30.0,
                sum: 60.0,
                count: 3
            }
        );
        assert_eq!((rows[1].host_id, rows[1].series_id), (1, 2));
        assert_eq!((rows[2].host_id, rows[2].series_id), (2, 1));
        assert_eq!(r.len(), 1, "下一個桶尚未結束");
    }

    #[test]
    fn take_ready_splits_only_between_hosts() {
        let mut r = Rollup::default();
        let b = bucket_start(T0, 300) * 1000;
        for host in 1..=3 {
            for series in 1..=4 {
                r.add(host, series, b, 1.0);
            }
        }
        let later = b + 3_600_000;
        // 上限 2 列：仍取完第一台主機的 4 列
        let first = r.take_ready(later, 2);
        assert_eq!(first.len(), 4);
        assert!(first.iter().all(|row| row.host_id == 1));
        let rest = r.take_ready(later, 5);
        assert_eq!(rest.len(), 8);
        assert!(r.take_ready(later, 5).is_empty());

        // 放回後與新的值合併
        r.put_back(first);
        r.add(1, 1, b, 3.0);
        let again = r.take_ready(later, 100);
        assert_eq!(again.len(), 4);
        assert_eq!(again[0].agg.count, 2);
        assert_eq!(again[0].agg.max, 3.0);
        r.clear();
        assert_eq!(r.len(), 0);
    }
}
