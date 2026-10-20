//! 彙總（rollup）：每 5 分鐘與每小時的 min／max／sum／count。
//!
//! 數值依 report 的**取樣時間**（`reports.ts`，見 `kairos/STORAGE.md`「時間」）分桶。寫入 report 的
//! 交易成功後，把每個數值加進記憶體中「尚未寫出」的 5 分鐘桶；桶結束 [`FLUSH_GRACE_SECS`] 秒、且
//! 該主機在這個桶已 [`FLUSH_GRACE_SECS`] 秒沒有新值之後寫出到 `rollup_5m`，同一個交易中再合併進
//! `rollup_1h`，並把該主機的 `hosts.rollup_until` 推進到桶的結束時間。
//!
//! 不變式：對每台主機，記憶體中的桶一定都在 `rollup_until` 之後，已寫出的桶一定都在之前
//! （每台主機的桶依時間順序寫出，見 [`Rollup::take_ready`]）。因此：
//! - 重新啟動時，各主機從 `rollup_until` 起重新讀取原始資料補回記憶體中的桶，不會漏算、也不會
//!   重複計算；
//! - 取樣時間早於 `rollup_until` 的遲到資料（durable 補送、client 時鐘偏差）不進記憶體，而是在寫入
//!   原始資料的同一個交易中直接合併進已寫出的桶（`ON CONFLICT DO UPDATE` 累加 min／max／sum／count）。

use std::collections::{BTreeMap, HashMap, HashSet};

/// 5 分鐘桶的長度（秒）
pub const BUCKET_5M_SECS: i64 = 300;
/// 1 小時桶的長度（秒）
pub const BUCKET_1H_SECS: i64 = 3600;
/// 桶結束後、以及主機在這個桶最後一次有新值後，再等這麼久才寫出：即時的 report 可能因排隊與限速
/// 晚幾秒才進入 writer；補送歷史資料時，正在補的桶持續有新值，等補完（移到下一個桶）再一次寫出，
/// 避免同一個桶被分成很多次寫出
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

    /// 合併另一個桶的統計值
    pub fn merge(&mut self, other: &Agg) {
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.sum += other.sum;
        self.count += other.count;
    }
}

/// 桶的開始時間（Unix 秒）
pub fn bucket_start(ts_ms: i64, bucket_secs: i64) -> i64 {
    ts_ms.div_euclid(1000).div_euclid(bucket_secs) * bucket_secs
}

/// 一組（5 分鐘桶, 主機, 序列）的統計值；寫出到資料庫的一列
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RollupRow {
    pub bucket: i64,
    pub host_id: i64,
    pub series_id: i64,
    pub agg: Agg,
}

/// 把 (host_id, series_id, ts_ms, 值) 依 5 分鐘桶合併成列（遲到資料直接寫進資料庫時使用）。
/// 非有限值不計
pub fn aggregate(samples: impl IntoIterator<Item = (i64, i64, i64, f64)>) -> Vec<RollupRow> {
    let mut map: BTreeMap<(i64, i64, i64), Agg> = BTreeMap::new();
    for (host_id, series_id, ts_ms, value) in samples {
        if !value.is_finite() {
            continue;
        }
        map.entry((bucket_start(ts_ms, BUCKET_5M_SECS), host_id, series_id))
            .and_modify(|a| a.add(value))
            .or_insert_with(|| Agg::new(value));
    }
    map.into_iter()
        .map(|((bucket, host_id, series_id), agg)| RollupRow {
            bucket,
            host_id,
            series_id,
            agg,
        })
        .collect()
}

/// 記憶體中尚未寫出的 5 分鐘桶。key 為 (桶開始的 Unix 秒, host_id, series_id)，
/// 依桶、主機的順序寫出（見 [`Rollup::take_ready`]）
#[derive(Debug, Default)]
pub struct Rollup {
    open: BTreeMap<(i64, i64, i64), Agg>,
    /// (桶, host_id) 最後一次加入數值的時間（Unix 毫秒，writer 的時鐘）
    touched: HashMap<(i64, i64), i64>,
}

impl Rollup {
    /// 加入一個數值（`ts_ms` 為取樣時間，`now_ms` 為目前時間）
    pub fn add(&mut self, host_id: i64, series_id: i64, ts_ms: i64, value: f64, now_ms: i64) {
        if !value.is_finite() {
            return;
        }
        let bucket = bucket_start(ts_ms, BUCKET_5M_SECS);
        self.open
            .entry((bucket, host_id, series_id))
            .and_modify(|a| a.add(value))
            .or_insert_with(|| Agg::new(value));
        let t = self.touched.entry((bucket, host_id)).or_insert(now_ms);
        *t = (*t).max(now_ms);
    }

    /// 尚未寫出的列數
    pub fn len(&self) -> usize {
        self.open.len()
    }

    /// 是否可能有可以寫出的桶（最舊的桶已結束；是否真的可以寫出由 [`Rollup::take_ready`] 判斷）
    pub fn has_ready(&self, now_ms: i64) -> bool {
        self.open
            .first_key_value()
            .is_some_and(|(&(bucket, _, _), _)| has_ended(bucket, now_ms))
    }

    /// 取出可以寫出的桶中最多約 `limit` 列。可以寫出 = 桶已結束 [`FLUSH_GRACE_SECS`] 秒，且該主機在
    /// 這個桶已 [`FLUSH_GRACE_SECS`] 秒沒有新值。
    ///
    /// 每台主機只取「依時間順序的前段」：某台主機有一個桶還不能寫出時，它之後的桶都不取（其他主機
    /// 照常），寫出時才能把 `rollup_until` 推進到桶的結束時間，而不會越過仍在記憶體中的桶。
    /// 只在（桶, 主機）的邊界切開：同一主機同一個桶的列一定在同一批
    pub fn take_ready(&mut self, now_ms: i64, limit: usize) -> Vec<RollupRow> {
        let mut out: Vec<RollupRow> = Vec::new();
        let mut blocked: HashSet<i64> = HashSet::new();
        let mut group: Option<(i64, i64)> = None;
        for (&(bucket, host_id, series_id), agg) in &self.open {
            if !has_ended(bucket, now_ms) {
                break;
            }
            if group != Some((bucket, host_id)) {
                if blocked.contains(&host_id) {
                    continue;
                }
                if out.len() >= limit {
                    break;
                }
                let idle = self
                    .touched
                    .get(&(bucket, host_id))
                    .is_none_or(|&t| now_ms >= t.saturating_add(FLUSH_GRACE_SECS * 1000));
                if !idle {
                    blocked.insert(host_id);
                    continue;
                }
                group = Some((bucket, host_id));
            }
            out.push(RollupRow {
                bucket,
                host_id,
                series_id,
                agg: *agg,
            });
        }
        for row in &out {
            self.open.remove(&(row.bucket, row.host_id, row.series_id));
            self.touched.remove(&(row.bucket, row.host_id));
        }
        out
    }

    /// 寫出失敗時放回（與之後加入的值合併）
    pub fn put_back(&mut self, rows: Vec<RollupRow>) {
        for row in rows {
            self.open
                .entry((row.bucket, row.host_id, row.series_id))
                .and_modify(|a| a.merge(&row.agg))
                .or_insert(row.agg);
        }
    }

    /// 捨棄一台主機尚未寫出的桶（刪除主機資料，`kairos/PRIVACY.md` 第 4 節）；回傳捨棄的列數
    pub fn remove_host(&mut self, host_id: i64) -> usize {
        let before = self.open.len();
        self.open.retain(|&(_, h, _), _| h != host_id);
        self.touched.retain(|&(_, h), _| h != host_id);
        before - self.open.len()
    }

    /// 清空（資料庫被換掉時，host_id 與 series_id 都不再有效；或改由原始資料重建）
    pub fn clear(&mut self) {
        self.open.clear();
        self.touched.clear();
    }
}

fn has_ended(bucket: i64, now_ms: i64) -> bool {
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
        // 即時資料：收到的時間與取樣時間相同
        r.add(1, 1, b, 10.0, b);
        r.add(1, 1, b + 1000, 30.0, b + 1000);
        r.add(1, 1, b + 299_999, 20.0, b + 299_999);
        r.add(1, 2, b, 5.0, b);
        r.add(2, 1, b, 7.0, b);
        r.add(1, 1, b + 300_000, 1.0, b + 300_000); // 下一個桶
        r.add(1, 1, b, f64::NAN, b); // 非有限值不計
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
                r.add(host, series, b, 1.0, b);
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
        r.add(1, 1, b, 3.0, b);
        let again = r.take_ready(later, 100);
        assert_eq!(again.len(), 4);
        assert_eq!(again[0].agg.count, 2);
        assert_eq!(again[0].agg.max, 3.0);
        r.clear();
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn a_bucket_being_backfilled_waits_and_blocks_only_its_own_host() {
        // 主機 1 正在補送 3 天前的資料（桶早已結束，但仍持續有新值）；主機 2 是即時資料
        let now = T0 + 3 * 86_400_000;
        let old = bucket_start(T0, 300) * 1000;
        let mut r = Rollup::default();
        r.add(1, 1, old, 1.0, now - 20_000); // 第一個舊桶：已閒置 20 秒
        r.add(1, 1, old + 300_000, 2.0, now - 1_000); // 第二個舊桶：1 秒前還有新值
        r.add(1, 1, old + 600_000, 3.0, now - 30_000); // 更晚的桶（不可越過第二個桶寫出）
        let live = bucket_start(now - 400_000, 300) * 1000;
        r.add(2, 1, live, 9.0, live + 1_000);
        let rows = r.take_ready(now, 100);
        let keys: Vec<(i64, i64)> = rows.iter().map(|r| (r.host_id, r.bucket)).collect();
        assert_eq!(
            keys,
            vec![(1, old / 1000), (2, live / 1000)],
            "主機 1 只寫出閒置的第一個桶，之後的桶等第二個桶補完；主機 2 不受影響"
        );
        assert!(r.has_ready(now));
        assert!(r.take_ready(now, 100).is_empty());
        // 第二個桶閒置 15 秒後，主機 1 的兩個桶依序寫出
        let rows = r.take_ready(now - 1_000 + FLUSH_GRACE_SECS * 1000, 100);
        assert_eq!(
            rows.iter().map(|r| r.bucket).collect::<Vec<_>>(),
            vec![old / 1000 + 300, old / 1000 + 600]
        );
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn removing_a_host_drops_only_its_buckets() {
        let mut r = Rollup::default();
        r.add(1, 1, T0, 1.0, T0);
        r.add(1, 2, T0, 1.0, T0);
        r.add(2, 1, T0, 1.0, T0);
        assert_eq!(r.remove_host(1), 2);
        assert_eq!(r.len(), 1);
        assert_eq!(r.remove_host(1), 0);
        let rows = r.take_ready(T0 + 3_600_000, 100);
        assert!(rows.iter().all(|row| row.host_id == 2) && rows.len() == 1);
    }

    #[test]
    fn late_samples_are_aggregated_per_bucket() {
        let b = bucket_start(T0, 300) * 1000;
        let rows = aggregate([
            (1, 1, b, 1.0),
            (1, 1, b + 10_000, 5.0),
            (1, 1, b + 300_000, 7.0),
            (1, 2, b, f64::INFINITY),
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].agg,
            Agg {
                min: 1.0,
                max: 5.0,
                sum: 6.0,
                count: 2
            }
        );
        assert_eq!(rows[1].bucket, b / 1000 + 300);
    }
}
