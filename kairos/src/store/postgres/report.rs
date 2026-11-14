//! PostgreSQL 後端的 report 寫入路徑（[`PgStore`]，[`ReportStore`]；`kairos/HA.md` 第 7 節）。
//!
//! 行為逐項對照 SQLite 後端（[`crate::store::sqlite::SqliteStore`]）：原始 report 每筆一列、主機／序列／boot
//! 的快取、每台主機的序列數上限、去重、行程快照、計數器速率與推算值、彙總（記憶體中的 5 分鐘桶，
//! [`Rollup`]）、遲到資料直接併入已寫出的桶、主機清冊同步、保留期限、總容量上限、日期保護、主機資產與
//! 刪除主機資料。所有方法都是同步的，只在 writer 的 blocking 執行緒上呼叫。
//!
//! 與 SQLite 後端不同的地方：
//!
//! - **連線**：一條不經過連線池的專用連線（[`PgPool::dedicated`]），帶 `statement_timeout`
//!   （`timeouts.write_secs`，HA.md 第 7.1 節）與 session 層級的共享 advisory lock（[`LockKind::Service`]：
//!   「有 Kairos 正在使用這個資料庫」）。連線中斷時在下一次使用前重新建立並重新載入快取；沒有檔案身分、
//!   背景 checkpoint 與 WAL。
//! - **一批的寫入**：`reports` 以一個多列的 `INSERT … SELECT FROM unnest(…) ON CONFLICT … DO NOTHING
//!   RETURNING` 寫入，由回傳的去重鍵分辨「真的寫入」與「重複」（交易中不能靠捕捉唯一鍵錯誤：出錯後整個交易
//!   作廢）。同一批內重複的去重鍵在送出前就判為 `Duplicate`。行程快照、遲到資料的彙總、彙總的寫出也都是
//!   多列的敘述。
//! - **交易**：一般的交易（READ COMMITTED），不經過 [`TxMode::Immediate`] 的全域寫入鎖。死結（`40P01`）與
//!   序列化失敗（`40001`）時整批自動重試最多 [`MAX_RETRIES`] 次；commit 之前的錯誤為 `Failed`，commit 時
//!   伺服器沒有回應（連線中斷）為 `Unknown`。
//! - **彙總**：沿用記憶體桶的演算法（與 SQLite 後端的輸出完全相同）。HA.md 第 7.3 節「leader 從原始資料
//!   產生彙總」是多個節點同時寫入（階段 B）才需要的，尚未實作；單一 writer 時兩者的結果相同。
//! - **總容量上限**（HA.md 第 7.5 節）：使用量是目前 schema 所有資料表（含索引與 TOAST）的
//!   `pg_total_relation_size` 加總，每 [`CAPACITY_CHECK_INTERVAL`] 檢查一次。PostgreSQL 刪除資料後檔案
//!   **不會變小**（空間由 VACUUM 標為可重複使用），所以不能像 SQLite 那樣「刪到量測值低於 95%」——那會把
//!   資料全部刪光。規則改為：
//!   1. 使用量超過門檻時開始一輪刪除。門檻是 `max(上限, 上一輪結束時的使用量 + 上限的 1%)`：上一輪刪出的
//!      空間被新資料用完之前檔案不會再長大，長大了才表示實際的資料量又回到上限附近。上一輪結束時的使用量
//!      保存在 `meta.capacity_high_water`，重新啟動後不會因為檔案仍然很大而重複刪除。
//!   2. 這一輪要刪除的量（bytes）＝ 使用量 − 上限的 95%。每張表每列的大小以「資料表大小 ÷ 列數」估計
//!      （先 `ANALYZE`，列數取 `pg_class.reltuples`），每刪一批就扣掉「列數 × 每列大小」，扣完就停止。
//!      刪除的順序與 SQLite 相同：最舊的原始 report（與對應的行程快照）→ 5 分鐘彙總 → 1 小時彙總。
//!   3. 一輪結束後在交易外對這四張表執行 `VACUUM (ANALYZE)`，讓刪出的空間可以重複使用，再量一次使用量
//!      作為新的門檻基準。
//!
//!   限制：這是估計值——索引膨脹或 autovacuum 來不及回收時，檔案會在實際資料量低於上限時就長大，造成
//!   比必要更早的刪除；使用量（`capacity_event.used_bytes`）是檔案大小而不是實際資料量，刪除後不會下降。
//!   需要讓檔案變小時由 DBA 在離峰時段執行 `VACUUM FULL` 或 `pg_repack`。

use chrono::{DateTime, NaiveDate, Utc};
use log::{debug, info, warn};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{Error, ErrorKind};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::conn::{LockKind, OptionalExtension, PgConn, PgError, PgResult, Value, array, params};
use super::pool::PgPool;
use crate::protocol::{
    HostInfo, METRIC_CPU_USAGE, METRIC_MEMORY_TOTAL, METRIC_MEMORY_USED, Metric,
};
use crate::registry::{LastReport, OFFLINE_RETENTION, RegistrySnapshot, RestoredAgent};
use crate::retention::log_guard_event;
use crate::storage::{Clock, RecordBody, ReportRecord, WriteStatus};
use crate::store::assets::{self, Change, MAX_CHANGES_PER_HOST};
use crate::store::codec::{
    self, AgentTs, BYTES_PER_MB, FORMAT_LEGACY, FORMAT_METRICS, canonical_labels,
};
use crate::store::db::TxMode;
use crate::store::derive;
use crate::store::purge::PurgeJob;
use crate::store::rollup::{
    Agg, BUCKET_1H_SECS, BUCKET_5M_SECS, Rollup, RollupRow, aggregate, bucket_start,
};
use crate::store::sqlite::{
    CAPACITY_LOW_WATERMARK_PERCENT, CODE_CAPACITY, CODE_SERIES_CAPPED, CapacityEvent,
    DELETE_BATCH_ROWS, FailPoint, HOSTS_FULL, INVENTORY_CHUNK_ROWS, InventoryRow,
    LAST_SEEN_WRITE_INTERVAL_MS, MAX_SERIES, META_CAPACITY_EVENT, META_RETENTION_GUARD,
    META_RETENTION_LAST, RETENTION_INTERVAL, ROLLUP_CHUNK_ROWS, RetentionRun, SCHEMA_VERSION,
    SERIES_CAP_LOG_INTERVAL, SERIES_FULL, SqliteOptions, series_key_derived,
};
use crate::store::{AssetOutcome, AssetWrite, ReportStore, sample_time_ms};
use crate::throttle::{Category, LogThrottle};
use crate::throttled;
use kairos_proto::{Inventory, inventory_digest};

/// 預設的敘述逾時（`timeouts.write_secs` 的預設值；由 [`PgStore::with_write_timeout`] 改為設定值）
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// writer 的連線等待鎖（session lock、列鎖）的上限
const WRITER_BUSY: Duration = Duration::from_secs(5);
/// 死結或序列化失敗時整批自動重試的次數上限（HA.md 第 5.4、7.1 節）
const MAX_RETRIES: u32 = 3;
/// 檢查總容量的間隔（`pg_total_relation_size` 的成本比 SQLite 的 PRAGMA 高；HA.md 第 7.5 節）
const CAPACITY_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// `meta` 的鍵：上一輪容量刪除結束時的使用量（bytes，文字）
const META_CAPACITY_HIGH_WATER: &str = "capacity_high_water";
/// 估計每列大小的下限（bytes）：統計資訊不可靠時不會因為估得太小而刪除過多
const MIN_ROW_BYTES: u64 = 64;
/// 日誌中稱呼 PostgreSQL 中的資料的名稱（DateGuard 的事件訊息）
const GUARD_LABEL: &str = " PostgreSQL 資料";
/// 刪除開始後多久之內拒絕這台主機的寫入（與 SQLite 後端相同）
const PURGE_HOLD: Duration = Duration::from_secs(10 * 60);
/// 寫入被拒的原因（連線回 `storage_unavailable`）
const PURGING: &str = "這台主機的資料正在刪除（或剛刪除），不接受它的資料";
/// 容量刪除涉及的資料表：估計每列大小的索引與 [`PgCapacity::row_bytes`] 相同
const CAPACITY_TABLES: [&str; 4] = ["reports", "rollup_5m", "rollup_1h", "process_snapshots"];

/// 目前 schema 所有資料表（含索引與 TOAST）的大小
const USED_BYTES_SQL: &str = "SELECT COALESCE(SUM(pg_total_relation_size(c.oid)), 0)::bigint
     FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = current_schema() AND c.relkind = 'r'";

/// 主機在快取中的資料
#[derive(Debug, Clone)]
struct HostEntry {
    host_id: i64,
    /// 已寫出彙總的時間（Unix 毫秒）：重新啟動時從這裡重新讀取原始資料補回彙總
    rollup_until: i64,
    /// 上次同步到清冊的內容（雜湊、last_seen）；尚未同步時為 `None`
    synced: Option<(u64, i64)>,
}

/// 衍生計算需要的序列資訊（[`derive::Role`] 不是 `Other` 的序列）
#[derive(Debug, Clone)]
struct SeriesInfo {
    name: String,
    unit: String,
    labels: String,
    role: derive::Role,
}

/// 主機、序列與 boot 的快取。交易失敗時整個清空、下一次寫入前從資料庫重新載入
/// （失敗的交易中新建立的編號已不存在）
#[derive(Debug, Default)]
struct Caches {
    loaded: bool,
    hosts: HashMap<String, HostEntry>,
    /// `name \x1f unit \x1f labels`（衍生序列另加 `\x1f derived`）→ series_id
    series: HashMap<String, i64>,
    /// 計數器、uptime、檔案系統容量等序列（衍生計算用；其他序列不在這裡）
    roles: HashMap<i64, Arc<SeriesInfo>>,
    /// 每台主機用過的序列（`host_series`）
    host_series: HashMap<i64, HashSet<i64>>,
    /// 每台主機各計數器序列的上一筆（速率計算）；已載入「上一筆」的主機在 `seeded` 中
    counters: HashMap<i64, derive::CounterState>,
    seeded: HashSet<i64>,
    /// 每台主機最近一次使用的 boot：(boot_id, boot_ref)
    boots: HashMap<i64, (String, i64)>,
    /// v1 三欄位對應的標準序列：cpu、memory.total、memory.used
    legacy: Option<[i64; 3]>,
    /// 這個交易中新增、尚未寫入 `host_series` 的（主機, 序列）：提交前以一個敘述寫入
    /// （[`Caches::flush_admitted`]）。新主機的第一筆 report 有十幾個新序列，逐筆寫入時每個都是一次往返
    admitted: Vec<(i64, i64)>,
}

impl Caches {
    fn load(conn: &PgConn) -> PgResult<Self> {
        let mut caches = Caches::default();
        for row in conn.query_rows("SELECT agent_id, host_id, rollup_until FROM hosts", ())? {
            caches.hosts.insert(
                row.get::<_, String>(0)?,
                HostEntry {
                    host_id: row.get(1)?,
                    rollup_until: row.get(2)?,
                    synced: None,
                },
            );
        }
        for row in conn.query_rows(
            "SELECT series_id, name, unit, labels, derived FROM series",
            (),
        )? {
            let id: i64 = row.get(0)?;
            let name: String = row.get(1)?;
            let unit: String = row.get(2)?;
            let labels: String = row.get(3)?;
            let derived: i64 = row.get(4)?;
            caches
                .series
                .insert(series_key_derived(&name, &unit, &labels, derived), id);
            caches.note_role(id, name, unit, labels, derived);
        }
        for row in conn.query_rows("SELECT host_id, series_id FROM host_series", ())? {
            caches
                .host_series
                .entry(row.get(0)?)
                .or_default()
                .insert(row.get(1)?);
        }
        caches.loaded = true;
        Ok(caches)
    }

    fn note_role(&mut self, id: i64, name: String, unit: String, labels: String, derived: i64) {
        let role = derive::role_of(&name, &unit, derived);
        if role != derive::Role::Other {
            self.roles.insert(
                id,
                Arc::new(SeriesInfo {
                    name,
                    unit,
                    labels,
                    role,
                }),
            );
        }
    }

    /// 一批 report 中第一次出現的主機：以一個敘述在清冊中新增（新主機大量出現時——第一次啟動、整批
    /// 註冊——逐台新增每台要兩次往返，1 萬台會讓寫入落後十幾秒）。超過 `max_hosts` 的不新增，之後
    /// [`Caches::host_id_of`] 會回 `None`。剛新增的主機沒有更早的 report，計數器不需要查「上一筆」
    fn create_hosts(
        &mut self,
        tx: &PgConn,
        records: &[ReportRecord],
        opts: &SqliteOptions,
    ) -> PgResult<()> {
        let room = opts.max_hosts.saturating_sub(self.hosts.len());
        let mut seen: HashSet<&str> = HashSet::new();
        let new: Vec<&ReportRecord> = records
            .iter()
            .filter(|r| !self.hosts.contains_key(r.agent_id.as_str()) && seen.insert(&r.agent_id))
            .take(room)
            .collect();
        // 只有一台時走原本的路徑（少一種寫法要驗證的情況）
        if new.len() < 2 {
            return Ok(());
        }
        let rows = tx.query_rows(
            "INSERT INTO hosts (agent_id, hostname, peer, identity_verified, online, first_seen, last_seen)
             SELECT u.a, u.h, u.p, ?4::bigint, 1, u.t, u.t
             FROM unnest(?1::text[], ?2::text[], ?3::text[], ?5::bigint[]) AS u(a, h, p, t)
             ON CONFLICT (agent_id) DO NOTHING
             RETURNING agent_id, host_id, rollup_until",
            vec![
                array(new.iter().map(|r| r.agent_id.as_str())),
                array(new.iter().map(|r| r.hostname.as_str())),
                array(new.iter().map(|r| r.peer.as_str())),
                Value::Integer(i64::from(opts.mtls)),
                array(new.iter().map(|r| r.recv_at.timestamp_millis())),
            ],
        )?;
        for row in rows {
            let agent_id: String = row.get(0)?;
            let host_id: i64 = row.get(1)?;
            self.seeded.insert(host_id);
            self.hosts.insert(
                agent_id,
                HostEntry {
                    host_id,
                    rollup_until: row.get(2)?,
                    synced: None,
                },
            );
        }
        Ok(())
    }

    /// 主機編號；第一次出現的主機在清冊中新增一列。清冊已滿時為 `None`
    fn host_id_of(
        &mut self,
        tx: &PgConn,
        agent_id: &str,
        hostname: &str,
        peer: &str,
        ts_ms: i64,
        opts: &SqliteOptions,
    ) -> PgResult<Option<i64>> {
        if let Some(e) = self.hosts.get(agent_id) {
            return Ok(Some(e.host_id));
        }
        if self.hosts.len() >= opts.max_hosts {
            return Ok(None);
        }
        tx.execute(
            "INSERT INTO hosts (agent_id, hostname, peer, identity_verified, online, first_seen, last_seen)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5) ON CONFLICT (agent_id) DO NOTHING",
            params![agent_id, hostname, peer, opts.mtls, ts_ms],
        )?;
        let (host_id, rollup_until) = tx.query_row(
            "SELECT host_id, rollup_until FROM hosts WHERE agent_id = ?1",
            [agent_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        self.hosts.insert(
            agent_id.to_string(),
            HostEntry {
                host_id,
                rollup_until,
                synced: None,
            },
        );
        Ok(Some(host_id))
    }

    /// 序列編號（原始序列）；第一次出現時新增。數量已達 [`MAX_SERIES`] 時為 `None`
    fn series_id(
        &mut self,
        tx: &PgConn,
        name: &str,
        unit: &str,
        labels: &str,
    ) -> PgResult<Option<i64>> {
        self.series_id_derived(tx, name, unit, labels, derive::DERIVED_NONE)
    }

    /// 序列編號（`derived` 見 [`derive::DERIVED_RATE`] 等）；第一次出現時新增
    fn series_id_derived(
        &mut self,
        tx: &PgConn,
        name: &str,
        unit: &str,
        labels: &str,
        derived: i64,
    ) -> PgResult<Option<i64>> {
        let key = series_key_derived(name, unit, labels, derived);
        if let Some(&id) = self.series.get(key.as_str()) {
            return Ok(Some(id));
        }
        if self.series.len() >= MAX_SERIES {
            return Ok(None);
        }
        tx.execute(
            "INSERT INTO series (name, unit, labels, derived) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (name, unit, labels, derived) DO NOTHING",
            params![name, unit, labels, derived],
        )?;
        let id: i64 = tx.query_row(
            "SELECT series_id FROM series WHERE name = ?1 AND unit = ?2 AND labels = ?3 AND derived = ?4",
            params![name, unit, labels, derived],
            |r| r.get(0),
        )?;
        self.series.insert(key, id);
        self.note_role(
            id,
            name.to_string(),
            unit.to_string(),
            labels.to_string(),
            derived,
        );
        Ok(Some(id))
    }

    /// 這台主機可以使用這個序列：已用過、或尚未達 `cap`（新序列寫入 `host_series`）。
    /// 已達上限時回傳 `false`（這個指標不保存）
    fn admit(&mut self, host_id: i64, series_id: i64, cap: usize) -> bool {
        let set = self.host_series.entry(host_id).or_default();
        if set.contains(&series_id) {
            return true;
        }
        if set.len() >= cap {
            return false;
        }
        set.insert(series_id);
        self.admitted.push((host_id, series_id));
        true
    }

    /// 這台主機還能收下「對它而言是新的」序列（尚未達每主機上限 `cap`）。安全稽核 N8：全域 `series`
    /// 表的新列只在這台主機還有空間時才建立，單一主機因此最多讓全域表增加 `cap` 筆，不會塞爆
    /// [`MAX_SERIES`] 讓其他主機的 report 被拒
    fn host_has_room(&self, host_id: i64, cap: usize) -> bool {
        self.host_series
            .get(&host_id)
            .map_or(cap > 0, |set| set.len() < cap)
    }

    /// 全域 `series` 快取是否已有這個序列（唯讀，不新增）
    fn has_series(&self, name: &str, unit: &str, labels: &str) -> bool {
        self.has_series_derived(name, unit, labels, derive::DERIVED_NONE)
    }

    /// 同 [`Self::has_series`]，含衍生序列
    fn has_series_derived(&self, name: &str, unit: &str, labels: &str, derived: i64) -> bool {
        self.series
            .contains_key(series_key_derived(name, unit, labels, derived).as_str())
    }

    /// 把這個交易中新增的（主機, 序列）寫入 `host_series`（一個敘述）。每個會呼叫 [`Caches::admit`] 的
    /// 交易在提交前呼叫；交易失敗時整個快取（含尚未寫入的部分）會被清空重載
    fn flush_admitted(&mut self, tx: &PgConn) -> PgResult<()> {
        if self.admitted.is_empty() {
            return Ok(());
        }
        tx.execute(
            "INSERT INTO host_series (host_id, series_id)
             SELECT * FROM unnest(?1::bigint[], ?2::bigint[]) ON CONFLICT DO NOTHING",
            vec![
                array(self.admitted.iter().map(|p| p.0)),
                array(self.admitted.iter().map(|p| p.1)),
            ],
        )?;
        self.admitted.clear();
        Ok(())
    }

    /// v1 的三個欄位在彙總中對應的標準序列（記憶體換算成 bytes，與 v2 的指標合併）
    fn legacy_series(&mut self, tx: &PgConn) -> PgResult<Option<[i64; 3]>> {
        if let Some(ids) = self.legacy {
            return Ok(Some(ids));
        }
        let mut ids = [0; 3];
        for (slot, (name, unit)) in ids.iter_mut().zip([
            (METRIC_CPU_USAGE, "%"),
            (METRIC_MEMORY_TOTAL, "By"),
            (METRIC_MEMORY_USED, "By"),
        ]) {
            match self.series_id(tx, name, unit, "")? {
                Some(id) => *slot = id,
                None => return Ok(None),
            }
        }
        self.legacy = Some(ids);
        Ok(Some(ids))
    }

    /// 去重鍵的 boot 編號（每台主機每個 boot_id 一列）
    fn boot_ref(&mut self, tx: &PgConn, host_id: i64, boot_id: &str) -> PgResult<i64> {
        if let Some((cached, boot_ref)) = self.boots.get(&host_id)
            && cached == boot_id
        {
            return Ok(*boot_ref);
        }
        tx.execute(
            "INSERT INTO boots (host_id, boot_id) VALUES (?1, ?2) ON CONFLICT (host_id, boot_id) DO NOTHING",
            params![host_id, boot_id],
        )?;
        let boot_ref: i64 = tx.query_row(
            "SELECT boot_ref FROM boots WHERE host_id = ?1 AND boot_id = ?2",
            params![host_id, boot_id],
            |r| r.get(0),
        )?;
        self.boots.insert(host_id, (boot_id.to_string(), boot_ref));
        Ok(boot_ref)
    }

    /// 已解碼的原始資料轉成衍生計算的輸入；只保留計數器、uptime、檔案系統容量等序列
    fn derive_items<'a>(
        &'a self,
        decoded: &'a [(i64, serde_json::Number)],
    ) -> Vec<derive::Item<'a>> {
        decoded
            .iter()
            .filter_map(|(id, v)| {
                let info = self.roles.get(id)?;
                Some(derive::Item {
                    id: *id,
                    name: &info.name,
                    unit: &info.unit,
                    labels: &info.labels,
                    role: info.role,
                    value: v,
                })
            })
            .collect()
    }

    /// 是否為標準計數器（v8 起原始累計值不彙總）
    fn is_counter(&self, id: i64) -> bool {
        self.roles
            .get(&id)
            .is_some_and(|i| i.role == derive::Role::Counter)
    }

    /// 這台主機的計數器狀態；第一次使用時以 `before_ms` 之前最近的一筆原始資料為「上一筆」
    /// （Kairos 重新啟動後；只查一次）。`before_id`：只看 id 小於它的列——一批的 report 先全部插入才
    /// 計算速率，這一批自己的列不能當成「上一筆」（與 SQLite 後端逐筆插入時看到的相同）
    fn seed_counters(
        &mut self,
        tx: &PgConn,
        host_id: i64,
        before_ms: i64,
        before_id: Option<i64>,
    ) -> PgResult<()> {
        if !self.seeded.insert(host_id) {
            return Ok(());
        }
        let prev: Option<(i64, Vec<u8>)> = tx
            .query_row(
                "SELECT ts, body FROM reports
                 WHERE host_id = ?1 AND ts < ?2 AND format = ?3 AND id < ?4
                 ORDER BY ts DESC LIMIT 1",
                params![
                    host_id,
                    before_ms,
                    FORMAT_METRICS,
                    before_id.unwrap_or(i64::MAX)
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((ts, body)) = prev {
            let decoded = codec::decode_metrics(&body).unwrap_or_default();
            let mut state = derive::CounterState::new();
            derive::derive(&self.derive_items(&decoded), ts, &mut state);
            self.counters.insert(host_id, state);
        }
        Ok(())
    }
}

/// 一批寫入中、交易成功後才加入彙總的值：(host_id, series_id, ts_ms, 值)
type Sample = (i64, i64, i64, f64);

/// 寫入一批時的失敗
enum BatchError {
    /// 確定沒有寫入（交易已回復，或伺服器明確拒絕了 commit）
    Failed(PgError),
    /// commit 時伺服器沒有回應：無法確定是否已寫入
    Unknown(PgError),
}

impl BatchError {
    fn inner(&self) -> &PgError {
        match self {
            BatchError::Failed(e) | BatchError::Unknown(e) => e,
        }
    }
}

/// commit 的錯誤：伺服器回報了錯誤（有 SQLSTATE）＝交易沒有生效；否則結果不明
fn commit_error(e: PgError) -> BatchError {
    if e.sqlstate().is_some() {
        BatchError::Failed(e)
    } else {
        BatchError::Unknown(e)
    }
}

/// 一筆已準備好、等待插入的 report
struct Prepared<'a> {
    /// 在這一批中的位置
    idx: usize,
    host_id: i64,
    ts: i64,
    recv_ms: i64,
    format: i64,
    body: Vec<u8>,
    agent_ts: AgentTs,
    /// 去重鍵：(boot_ref, report_seq)
    dedup: Option<(i64, i64)>,
    /// 保存的指標（序列編號、指標、正規化的 label）；v1 的 report 為空
    kept: Vec<(i64, &'a Metric, String)>,
    /// v1 三個欄位的彙總值
    samples: Vec<Sample>,
}

/// 依保留期限刪除的進度（每小時一輪，每一步一個交易）
#[derive(Debug)]
struct RetentionJob {
    stage: RetentionStage,
    hosts: Vec<i64>,
    cursor: usize,
    /// 刪除 `ts`（毫秒）早於此值的原始 report
    raw_before_ms: Option<i64>,
    /// 刪除 `bucket`（秒）早於此值的彙總
    rollup_5m_before: Option<i64>,
    rollup_1h_before: Option<i64>,
    /// 刪除的列數：原始、5 分鐘、1 小時、boot、行程快照、資產變更歷史
    deleted: [u64; 6],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetentionStage {
    Raw,
    /// 行程快照（保留期限同原始資料）
    Processes,
    Rollup5m,
    Rollup1h,
    /// 資產變更歷史（保留期限同 1 小時彙總）
    Changes,
    Boots,
}

/// 總容量上限的狀態（規則見本檔開頭的說明）
#[derive(Debug, Default)]
struct PgCapacity {
    /// 已超過門檻、正在刪除
    active: bool,
    next_check: Option<Instant>,
    /// 這一輪刪除的列數：原始、5 分鐘、1 小時
    deleted: [u64; 3],
    /// 這一輪開始的時間（Unix 毫秒）
    started_at: i64,
    /// 這一輪還要刪除的量（bytes，估計值）
    budget: i64,
    /// 每列的估計大小：原始 report、5 分鐘彙總、1 小時彙總、行程快照（[`CAPACITY_TABLES`] 的順序）
    row_bytes: [u64; 4],
    /// 上一輪結束時的使用量（`meta.capacity_high_water`）；`None`：還沒讀取
    high_water: Option<u64>,
}

fn usize_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn usize_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// 寫入 `meta`（失敗只影響系統健康的顯示，不影響寫入）
fn put_meta(conn: &PgConn, key: &str, value: &impl serde::Serialize) -> PgResult<()> {
    let text = serde_json::to_string(value).unwrap_or_default();
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, text],
    )
    .map(|_| ())
}

/// PostgreSQL 後端
pub struct PgStore {
    pool: Arc<PgPool>,
    /// 顯示用的位置（不含密碼）
    target: String,
    opts: SqliteOptions,
    clock: Clock,
    write_timeout: Duration,
    conn: Option<PgConn>,
    caches: Caches,
    rollup: Rollup,
    guard: Option<date_guard::DateGuard>,
    retention: Option<RetentionJob>,
    next_retention: Option<Instant>,
    capacity: PgCapacity,
    throttle: LogThrottle,
    /// 提交結果不明之後：記憶體中的彙總可能少了已提交的那一批，下一次使用前改由原始資料重建
    rollup_stale: bool,
    /// 主機清冊中尚未寫入的列（最新一份快照中有變化的主機），每次維護步驟寫入一批
    pending_inventory: VecDeque<InventoryRow>,
    /// 每台主機上次記錄「序列數已達上限」的時間（`KRS-W4104`）
    series_cap_logged: HashMap<String, Instant>,
    /// 正在刪除或剛刪除資料的主機（`agent_id` → 開始的時間）：這段期間不寫入它的 report、資產與清冊
    purging: HashMap<String, Instant>,
    /// 測試用的失敗注入
    pub(crate) fail_next: Option<FailPoint>,
}

impl std::fmt::Debug for PgStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgStore")
            .field("target", &self.target)
            .field("open", &self.conn.is_some())
            .field("hosts", &self.caches.hosts.len())
            .field("rollup_pending", &self.rollup.len())
            .finish()
    }
}

impl PgStore {
    /// 連線（必要時建立 schema），回傳 store 與要放回狀態表的主機（最近 24 小時出現過的，見
    /// `Registry::restore`）。失敗時 Kairos 不啟動。`opts` 沿用 [`SqliteOptions`]（`path`、`synchronous`
    /// 不使用）。
    ///
    /// 也會：把上次異常結束時仍標為線上的主機改為離線、從原始資料補回尚未寫出的彙總、讀回日期保護的狀態。
    pub fn open(
        pool: Arc<PgPool>,
        opts: SqliteOptions,
        clock: Clock,
    ) -> Result<(Self, Vec<RestoredAgent>), Error> {
        let target = pool.settings().display_target();
        let mut store = Self {
            pool,
            target,
            opts,
            clock,
            write_timeout: DEFAULT_WRITE_TIMEOUT,
            conn: None,
            caches: Caches::default(),
            rollup: Rollup::default(),
            guard: None,
            retention: None,
            next_retention: None,
            capacity: PgCapacity::default(),
            throttle: LogThrottle::default(),
            rollup_stale: false,
            pending_inventory: VecDeque::new(),
            series_cap_logged: HashMap::new(),
            purging: HashMap::new(),
            fail_next: None,
        };
        // 先自己連線：保留錯誤的種類（ResourceBusy、連線被拒等），啟動時據此分類
        store.conn = Some(store.connect()?);
        store.ensure_ready().map_err(Error::other)?;
        let now = (store.clock)();
        let target = store.target.clone();
        let fail = |what: &str, e: PgError| Error::other(format!("{target}：{what}: {e}"));
        let conn = store
            .conn
            .as_ref()
            .ok_or_else(|| Error::other("資料庫未連線"))?;
        let stale = conn
            .execute("UPDATE hosts SET online = 0 WHERE online = 1", ())
            .map_err(|e| fail("無法更新主機清冊", e))?;
        if stale > 0 {
            info!(
                "主機清冊中有 {stale} 台主機在上次結束時仍標為線上（Kairos 未正常關機），改為離線"
            );
        }
        let since = now - chrono::Duration::from_std(OFFLINE_RETENTION).unwrap_or_default();
        let restored = load_restored(conn, since.timestamp_millis())
            .map_err(|e| fail("無法讀取主機清冊", e))?;
        let recovered = store
            .recover_rollup()
            .map_err(|e| fail("無法從原始資料補回彙總", e))?;
        store.guard = Some(store.load_guard(now));
        info!(
            "PostgreSQL 資料庫 {}（schema v{}）：主機清冊 {} 台、指標序列 {} 個{}",
            store.target,
            SCHEMA_VERSION,
            store.caches.hosts.len(),
            store.caches.series.len(),
            if recovered > 0 {
                format!("；已從原始資料補回 {recovered} 筆 report 的彙總")
            } else {
                String::new()
            }
        );
        Ok((store, restored))
    }

    /// 設定連線的 `statement_timeout`（`timeouts.write_secs`）；已連線時立即套用，重新連線時也會套用
    pub fn with_write_timeout(mut self, timeout: Duration) -> Self {
        self.write_timeout = timeout;
        if let Some(conn) = self.conn.as_ref()
            && let Err(e) = Self::apply_timeout(conn, timeout)
        {
            warn!("無法設定 PostgreSQL 連線的 statement_timeout: {e}");
        }
        self
    }

    /// 測試與量測：目前的連線（直接查看資料表）
    #[allow(dead_code)]
    pub(crate) fn conn(&self) -> Option<&PgConn> {
        self.conn.as_ref()
    }

    fn apply_timeout(conn: &PgConn, timeout: Duration) -> PgResult<()> {
        conn.execute_batch(&format!(
            "SET statement_timeout = {}",
            timeout.as_millis().max(1)
        ))
    }

    /// 建立 writer 的專用連線：取得「服務使用中」的共享鎖、確認 schema、設定敘述逾時
    fn connect(&self) -> Result<PgConn, Error> {
        let conn = self.pool.dedicated(WRITER_BUSY)?;
        conn.session_lock_shared(LockKind::Service, WRITER_BUSY)
            .map_err(|e| {
                if e.sqlstate() == Some("55P03") {
                    Error::new(
                        ErrorKind::ResourceBusy,
                        format!(
                            "{}：資料庫正被 kairos 子命令獨占使用（例如 db migrate 或離線刪除主機資料），請等它結束後再啟動",
                            self.target
                        ),
                    )
                } else {
                    Error::other(format!("{}：無法取得資料庫的使用鎖: {e}", self.target))
                }
            })?;
        // 啟動保護：單機模式的 Kairos 不與其他 Kairos 共用 schema（重新連線時也重新取得）
        self.pool.claim_service_mode(&conn)?;
        super::ensure_schema(&conn)?;
        Self::apply_timeout(&conn, self.write_timeout).map_err(|e| {
            Error::other(format!("{}：無法設定 statement_timeout: {e}", self.target))
        })?;
        Ok(conn)
    }

    /// 確認連線可用：連線已中斷時重新建立，快取未載入時載入。失敗時回傳原因（這一批確定未寫入）。
    /// 重新連線後資料庫仍是同一個，記憶體中的彙總（主機與序列的編號）繼續有效
    fn ensure_ready(&mut self) -> Result<(), String> {
        if self.conn.as_ref().is_some_and(PgConn::is_broken) {
            warn!("與 PostgreSQL（{}）的連線已中斷，重新連線", self.target);
            self.conn = None;
            self.caches = Caches::default();
        }
        if self.conn.is_none() {
            let conn = self.connect().map_err(|e| e.to_string())?;
            self.conn = Some(conn);
            self.caches.loaded = false;
        }
        if !self.caches.loaded {
            let conn = self.conn.as_ref().ok_or("資料庫未連線")?;
            match Caches::load(conn) {
                Ok(c) => self.caches = c,
                Err(e) => {
                    self.conn = None;
                    return Err(format!("無法讀取 {} 的主機與序列: {}", self.target, e));
                }
            }
        }
        if self.rollup_stale {
            // 提交結果不明：那一批可能已寫入，記憶體中的彙總卻沒有它的值（重送時是 Duplicate，
            // 不會再加入）。以原始資料重建記憶體中的桶（與啟動時相同），之後才繼續寫入
            self.rollup.clear();
            match self.recover_rollup() {
                Ok(n) => {
                    self.rollup_stale = false;
                    info!("提交結果不明之後，已從原始資料重建尚未寫出的彙總（{n} 筆 report）");
                }
                Err(e) => {
                    self.invalidate(e.is_connection_fatal());
                    return Err(format!("無法從 {} 的原始資料重建彙總: {}", self.target, e));
                }
            }
        }
        Ok(())
    }

    /// 交易失敗後：清空快取（失敗交易中新建立的編號已不存在），下一次寫入前重新載入；
    /// `reopen` 時另關閉連線
    fn invalidate(&mut self, reopen: bool) {
        self.caches = Caches::default();
        if reopen || self.conn.as_ref().is_some_and(PgConn::is_broken) {
            self.conn = None;
        }
    }

    /// 正在刪除或剛刪除的主機的 report 標為失敗
    fn reject_purging(&mut self, records: &[ReportRecord], statuses: &mut [WriteStatus]) {
        if self.purging.is_empty() {
            return;
        }
        self.expire_purging();
        for (r, s) in records.iter().zip(statuses.iter_mut()) {
            if self.purging.contains_key(&r.agent_id) {
                *s = WriteStatus::Failed(PURGING.to_string());
            }
        }
    }

    fn expire_purging(&mut self) {
        self.purging.retain(|_, t| t.elapsed() < PURGE_HOLD);
    }

    /// 在一個交易中寫入一批。取樣時間不早於主機 `rollup_until` 的數值放進 `samples`（提交後才加入
    /// 記憶體中的桶）；早於的（遲到資料：對應的桶已寫出）在同一個交易中直接合併進彙總表。
    ///
    /// 分三段：(1) 逐筆決定主機、序列、boot 並編碼內容（只有快取未命中時才查資料庫）；(2) 一個敘述插入
    /// 所有 report，由回傳的去重鍵得知哪些真的寫入；(3) 對真的寫入的 report 依序計算速率與推算值、
    /// 收集彙總的數值與行程快照。重複的 report 不寫入、不加入彙總、不更新計數器的上一筆（與 SQLite 相同）
    fn insert_batch(
        &mut self,
        records: &[ReportRecord],
        statuses: &mut [WriteStatus],
        samples: &mut Vec<Sample>,
        capped: &mut Vec<(String, usize)>,
    ) -> Result<(), BatchError> {
        let Self {
            conn,
            caches,
            opts,
            fail_next,
            ..
        } = self;
        let before = BatchError::Failed;
        let conn = conn
            .as_ref()
            .ok_or_else(|| before(PgError::other("資料庫未連線")))?;
        let tx = conn.begin_tx(TxMode::Deferred).map_err(before)?;
        let cap = opts.max_series_per_host;

        // (1) 準備
        caches.create_hosts(&tx, records, opts).map_err(before)?;
        let mut prepared: Vec<Prepared<'_>> = Vec::with_capacity(records.len());
        let mut seen_keys: HashSet<(i64, i64)> = HashSet::new();
        let mut buf: Vec<u8> = Vec::with_capacity(1024);
        'records: for (i, r) in records.iter().enumerate() {
            // 正在刪除資料的主機（reject_purging 已標為失敗）
            if !matches!(statuses[i], WriteStatus::Written) {
                continue;
            }
            let recv_ms = r.recv_at.timestamp_millis();
            // 取樣時間：durable report 為 client 的 ts（補送的歷史資料），否則為收到的時間
            let ts = sample_time_ms(r);
            let Some(host_id) = caches
                .host_id_of(&tx, &r.agent_id, &r.hostname, &r.peer, recv_ms, opts)
                .map_err(before)?
            else {
                statuses[i] = WriteStatus::Failed(HOSTS_FULL.to_string());
                continue;
            };
            buf.clear();
            let mut kept: Vec<(i64, &Metric, String)> = Vec::new();
            let mut record_samples: Vec<Sample> = Vec::new();
            let format = match &r.body {
                RecordBody::Legacy {
                    cpu_usage,
                    total_memory,
                    used_memory,
                } => {
                    let Some([cpu, total, used]) = caches.legacy_series(&tx).map_err(before)?
                    else {
                        statuses[i] = WriteStatus::Failed(SERIES_FULL.to_string());
                        continue;
                    };
                    for id in [cpu, total, used] {
                        caches.admit(host_id, id, cap);
                    }
                    codec::encode_legacy(&mut buf, *cpu_usage, *total_memory, *used_memory);
                    record_samples.extend([
                        (host_id, cpu, ts, f64::from(*cpu_usage)),
                        (host_id, total, ts, (*total_memory * BYTES_PER_MB) as f64),
                        (host_id, used, ts, (*used_memory * BYTES_PER_MB) as f64),
                    ]);
                    FORMAT_LEGACY
                }
                RecordBody::Metrics { metrics } => {
                    let mut dropped = 0;
                    for m in metrics {
                        let labels = canonical_labels(&m.labels);
                        // N8：這台主機已達每主機上限時，對它而言是新的序列直接丟棄，不先寫進全域
                        // series 表（host_has_room 是 O(1)，只有已滿時才多查一次 has_series）
                        if !caches.host_has_room(host_id, cap)
                            && !caches.has_series(&m.name, &m.unit, &labels)
                        {
                            dropped += 1;
                            continue;
                        }
                        match caches
                            .series_id(&tx, &m.name, &m.unit, &labels)
                            .map_err(before)?
                        {
                            Some(id) => {
                                if caches.admit(host_id, id, cap) {
                                    kept.push((id, m, labels));
                                } else {
                                    dropped += 1;
                                }
                            }
                            None => {
                                statuses[i] = WriteStatus::Failed(SERIES_FULL.to_string());
                                continue 'records;
                            }
                        }
                    }
                    if dropped > 0 {
                        capped.push((r.agent_id.clone(), dropped));
                        // 主控台 S05 的提示（設計包 §30.5 #7）
                        tx.execute(
                            "UPDATE hosts SET series_limited_at = ?2 WHERE host_id = ?1",
                            params![host_id, recv_ms],
                        )
                        .map_err(before)?;
                    }
                    codec::encode_metrics(&mut buf, kept.iter().map(|(id, m, _)| (*id, &m.value)));
                    FORMAT_METRICS
                }
            };
            let dedup = match &r.dedup {
                Some(d) => {
                    let boot_ref = caches.boot_ref(&tx, host_id, &d.boot_id).map_err(before)?;
                    let key = (boot_ref, i64::try_from(d.report_seq).unwrap_or(i64::MAX));
                    if !seen_keys.insert(key) {
                        // 同一批中較早的一筆已有相同的去重鍵
                        statuses[i] = WriteStatus::Duplicate;
                        continue;
                    }
                    Some(key)
                }
                None => None,
            };
            prepared.push(Prepared {
                idx: i,
                host_id,
                ts,
                recv_ms,
                format,
                body: buf.clone(),
                agent_ts: AgentTs::parse(&r.ts),
                dedup,
                kept,
                samples: record_samples,
            });
        }

        // (2) 插入
        let mut written: HashSet<(i64, i64)> = HashSet::new();
        let mut first_id: Option<i64> = None;
        if !prepared.is_empty() {
            let int = |f: &dyn Fn(&Prepared<'_>) -> Option<i64>| {
                Value::Array(
                    prepared
                        .iter()
                        .map(|p| f(p).map_or(Value::Null, Value::Integer))
                        .collect(),
                )
            };
            let rows = tx
                .query_rows(
                    "INSERT INTO reports (host_id, ts, seq, agent_ts_ms, agent_ts_text, format, body,
                        boot_ref, report_seq, recv_lag)
                     SELECT * FROM unnest(?1::bigint[], ?2::bigint[], ?3::bigint[], ?4::bigint[],
                        ?5::text[], ?6::bigint[], ?7::bytea[], ?8::bigint[], ?9::bigint[], ?10::bigint[])
                     ON CONFLICT (boot_ref, report_seq) WHERE boot_ref IS NOT NULL DO NOTHING
                     RETURNING id, boot_ref, report_seq",
                    vec![
                        int(&|p| Some(p.host_id)),
                        int(&|p| Some(p.ts)),
                        int(&|p| Some(i64::try_from(records[p.idx].seq).unwrap_or(i64::MAX))),
                        int(&|p| match &p.agent_ts {
                            AgentTs::Millis(ms) => Some(*ms),
                            AgentTs::Text(_) => None,
                        }),
                        Value::Array(
                            prepared
                                .iter()
                                .map(|p| match &p.agent_ts {
                                    AgentTs::Millis(_) => Value::Null,
                                    AgentTs::Text(t) => Value::Text(t.clone()),
                                })
                                .collect(),
                        ),
                        int(&|p| Some(p.format)),
                        Value::Array(
                            prepared
                                .iter()
                                .map(|p| Value::Blob(p.body.clone()))
                                .collect(),
                        ),
                        int(&|p| p.dedup.map(|d| d.0)),
                        int(&|p| p.dedup.map(|d| d.1)),
                        int(&|p| (p.recv_ms != p.ts).then_some(p.recv_ms - p.ts)),
                    ],
                )
                .map_err(before)?;
            for row in &rows {
                let id: i64 = row.get(0).map_err(before)?;
                first_id = Some(first_id.map_or(id, |f| f.min(id)));
                if let (Some(b), Some(s)) = (
                    row.get::<_, Option<i64>>(1).map_err(before)?,
                    row.get::<_, Option<i64>>(2).map_err(before)?,
                ) {
                    written.insert((b, s));
                }
            }
        }

        // (3) 真的寫入的 report：速率與推算值、彙總、行程快照
        let mut late: Vec<Sample> = Vec::new();
        let mut snapshots: BTreeMap<(i64, i64), Vec<u8>> = BTreeMap::new();
        for p in prepared {
            if p.dedup.is_some_and(|key| !written.contains(&key)) {
                // 重複（已保存過）
                statuses[p.idx] = WriteStatus::Duplicate;
                continue;
            }
            let r = &records[p.idx];
            let (host_id, ts) = (p.host_id, p.ts);
            let mut record_samples = p.samples;
            if p.format == FORMAT_METRICS {
                caches
                    .seed_counters(&tx, host_id, ts, first_id)
                    .map_err(before)?;
                let items: Vec<derive::Item> = p
                    .kept
                    .iter()
                    .map(|(id, m, labels)| derive::Item {
                        id: *id,
                        name: &m.name,
                        unit: &m.unit,
                        labels,
                        role: derive::role_of(&m.name, &m.unit, derive::DERIVED_NONE),
                        value: &m.value,
                    })
                    .collect();
                // 量測值照常彙總；計數器只彙總速率
                record_samples.extend(
                    items
                        .iter()
                        .filter(|it| it.role != derive::Role::Counter)
                        .map(|it| (host_id, it.id, ts, it.value.as_f64().unwrap_or(f64::NAN))),
                );
                let state = caches.counters.entry(host_id).or_default();
                let points = derive::derive(&items, ts, state);
                for point in points {
                    // N8：這台主機已達上限時，對它而言是新的衍生序列不寫進全域 series 表
                    if !caches.host_has_room(host_id, cap)
                        && !caches.has_series_derived(
                            &point.name,
                            &point.unit,
                            &point.labels,
                            point.derived,
                        )
                    {
                        continue;
                    }
                    let Some(id) = caches
                        .series_id_derived(
                            &tx,
                            &point.name,
                            &point.unit,
                            &point.labels,
                            point.derived,
                        )
                        .map_err(before)?
                    else {
                        continue;
                    };
                    if caches.admit(host_id, id, cap) {
                        record_samples.push((host_id, id, ts, point.value));
                    }
                }
            }
            if let Some(processes) = &r.processes {
                // 同一批中同一台主機同一個取樣時間的快照：後面的取代前面的（與逐筆 upsert 相同）
                snapshots.insert(
                    (host_id, ts),
                    serde_json::to_vec(processes).unwrap_or_default(),
                );
            }
            let rollup_until = caches
                .hosts
                .get(r.agent_id.as_str())
                .map_or(0, |e| e.rollup_until);
            if ts < rollup_until {
                late.extend_from_slice(&record_samples);
            } else {
                samples.extend_from_slice(&record_samples);
            }
        }
        if !snapshots.is_empty() {
            tx.execute(
                "INSERT INTO process_snapshots (host_id, ts, body)
                 SELECT * FROM unnest(?1::bigint[], ?2::bigint[], ?3::bytea[])
                 ON CONFLICT (host_id, ts) DO UPDATE SET body = excluded.body",
                vec![
                    array(snapshots.keys().map(|k| k.0)),
                    array(snapshots.keys().map(|k| k.1)),
                    array(snapshots.values()),
                ],
            )
            .map_err(before)?;
        }
        if !late.is_empty() {
            merge_rollup_rows(&tx, &aggregate(late.drain(..))).map_err(before)?;
        }
        caches.flush_admitted(&tx).map_err(before)?;
        // 失敗注入（測試）：回傳錯誤時 tx 被釋放，交易回復
        let injected = || PgError::other("測試注入的失敗");
        match fail_next.take() {
            None => {}
            Some(FailPoint::BeforeCommit | FailPoint::CommitDiskFull) => {
                return Err(BatchError::Failed(injected()));
            }
            Some(FailPoint::CommitUnknown) => {
                tx.commit().map_err(commit_error)?;
                return Err(BatchError::Unknown(injected()));
            }
        }
        tx.commit().map_err(commit_error)
    }

    /// 每台主機的序列數已達上限、部分指標未保存（`KRS-W4104`；每台主機每小時最多記一次）
    fn log_series_capped(&mut self, capped: Vec<(String, usize)>) {
        let now = Instant::now();
        for (agent_id, dropped) in capped {
            let due = self
                .series_cap_logged
                .get(&agent_id)
                .is_none_or(|t| now.saturating_duration_since(*t) >= SERIES_CAP_LOG_INTERVAL);
            if !due {
                continue;
            }
            if self.series_cap_logged.len() >= 10_000 {
                self.series_cap_logged.clear();
            }
            self.series_cap_logged.insert(agent_id.clone(), now);
            warn!(
                "警告 {CODE_SERIES_CAPPED}：主機 {agent_id} 的指標序列數已達上限（{}，storage.max_series_per_host），部分指標未保存：這筆 report 中 {dropped} 個新序列的指標未保存，既有的序列照常保存。常見原因是網路介面名稱不斷變化（例如容器的虛擬介面）或大量自訂指標；處理方式：在這台主機的 pulse.toml 以 collect.interface_exclude 排除這些介面並重新啟動 Pulse，確實需要更多序列時調高 storage.max_series_per_host（每台主機每小時最多記錄一次）",
                self.opts.max_series_per_host
            );
        }
    }

    /// 寫入一批（一個交易）。回傳每筆的結果
    fn write(&mut self, records: &[ReportRecord]) -> Vec<WriteStatus> {
        if records.is_empty() {
            return Vec::new();
        }
        let mut attempt = 0;
        let (result, statuses, samples, capped) = loop {
            if let Err(reason) = self.ensure_ready() {
                throttled!(
                    self.throttle,
                    Category::StorageWrite,
                    error,
                    "{}，{} 筆 report 未寫入",
                    reason,
                    records.len()
                );
                return vec![WriteStatus::Failed(reason); records.len()];
            }
            let mut statuses = vec![WriteStatus::Written; records.len()];
            self.reject_purging(records, &mut statuses);
            let mut samples = Vec::new();
            let mut capped = Vec::new();
            let result = self.insert_batch(records, &mut statuses, &mut samples, &mut capped);
            // 死結或序列化失敗：交易已回復，整批重試（快取重新載入）
            let retryable = match &result {
                Err(BatchError::Failed(e)) if e.is_retryable() => {
                    Some(e.sqlstate().map(str::to_string))
                }
                _ => None,
            };
            match retryable {
                Some(state) if attempt < MAX_RETRIES => {
                    attempt += 1;
                    self.pool.count_retry(state.as_deref());
                    debug!(
                        "寫入 {} 時發生可重試的錯誤（SQLSTATE {}），第 {attempt} 次重試",
                        self.target,
                        state.as_deref().unwrap_or("?")
                    );
                    self.invalidate(false);
                }
                _ => break (result, statuses, samples, capped),
            }
        };
        match result {
            Ok(()) => {
                self.log_series_capped(capped);
                let now_ms = (self.clock)().timestamp_millis();
                for (host_id, series_id, ts, value) in samples {
                    self.rollup.add(host_id, series_id, ts, value, now_ms);
                }
                debug!(
                    "已寫入 {} 筆 report 至 {}",
                    statuses
                        .iter()
                        .filter(|s| **s == WriteStatus::Written)
                        .count(),
                    self.target
                );
                statuses
            }
            Err(BatchError::Failed(e)) => {
                // 交易已回復：確定沒有寫入
                let reason = format!("寫入 {} 失敗: {}", self.target, e);
                throttled!(
                    self.throttle,
                    Category::StorageWrite,
                    error,
                    "{}，本批 {} 筆 report 未寫入",
                    reason,
                    records.len()
                );
                self.invalidate(e.is_connection_fatal());
                vec![WriteStatus::Failed(reason); records.len()]
            }
            Err(unknown @ BatchError::Unknown(_)) => {
                let reason = format!("寫入 {} 時提交失敗: {}", self.target, unknown.inner());
                throttled!(
                    self.throttle,
                    Category::StorageWrite,
                    error,
                    "{}，本批 {} 筆 report 是否已保存無法確定（連線將不回應並關閉）",
                    reason,
                    records.len()
                );
                // 無法確定交易是否生效：重新連線並重新載入快取，記憶體中的彙總改由原始資料重建
                self.invalidate(true);
                self.rollup_stale = true;
                vec![WriteStatus::Unknown(reason); records.len()]
            }
        }
    }

    /// 讀回日期保護的狀態；沒有時以最新一筆 report 的日期（不晚於今天）為基準
    fn load_guard(&self, now: DateTime<Utc>) -> date_guard::DateGuard {
        let today = now.date_naive();
        let Some(conn) = self.conn.as_ref() else {
            return date_guard::DateGuard::new(today, Instant::now(), now);
        };
        let saved: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [META_RETENTION_GUARD],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        if let Some(saved) = saved {
            match date_guard::DateGuard::from_state(
                saved.as_bytes(),
                date_guard::LegacyState::Accept,
                Instant::now(),
                now,
            ) {
                Some(g) => return g,
                None => warn!("資料庫中的日期保護狀態無效，改以最新一筆 report 的日期為基準"),
            }
        }
        let newest: Option<i64> = conn
            .query_row(
                "SELECT ts + COALESCE(recv_lag, 0) FROM reports ORDER BY id DESC LIMIT 1",
                (),
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        let newest = newest
            .and_then(DateTime::from_timestamp_millis)
            .map(|t| t.date_naive());
        date_guard::DateGuard::new(
            date_guard::guard_baseline(newest, today),
            Instant::now(),
            now,
        )
    }

    /// 從各主機的 `rollup_until` 起（取樣時間）重新讀取原始資料，補回記憶體中尚未寫出的彙總。
    /// 早於 `rollup_until` 的資料已在寫出的桶中（含遲到資料的直接合併），不再讀取。回傳讀取的筆數
    fn recover_rollup(&mut self) -> PgResult<u64> {
        let now_ms = (self.clock)().timestamp_millis();
        let Self {
            conn,
            caches,
            rollup,
            opts,
            ..
        } = self;
        let Some(conn) = conn.as_ref() else {
            return Ok(0);
        };
        let tx = conn.begin_tx(TxMode::Deferred)?;
        let mut count = 0;
        let cap = opts.max_series_per_host;
        let hosts: Vec<(i64, i64)> = caches
            .hosts
            .values()
            .map(|e| (e.host_id, e.rollup_until))
            .collect();
        for (host_id, until) in hosts {
            // 計數器的上一筆：rollup_until 之前最近的一筆（不加入彙總）
            caches.seed_counters(&tx, host_id, until, None)?;
            let rows = tx.query_rows(
                "SELECT ts, format, body FROM reports WHERE host_id = ?1 AND ts >= ?2 ORDER BY ts",
                params![host_id, until],
            )?;
            for row in rows {
                let ts: i64 = row.get(0)?;
                let format: i64 = row.get(1)?;
                let body: Vec<u8> = row.get(2)?;
                count += 1;
                match format {
                    FORMAT_LEGACY => {
                        if let (Some((cpu, total, used)), Some([c, t, u])) =
                            (codec::decode_legacy(&body), caches.legacy_series(&tx)?)
                        {
                            rollup.add(host_id, c, ts, f64::from(cpu), now_ms);
                            rollup.add(host_id, t, ts, (total * BYTES_PER_MB) as f64, now_ms);
                            rollup.add(host_id, u, ts, (used * BYTES_PER_MB) as f64, now_ms);
                        }
                    }
                    FORMAT_METRICS => {
                        let decoded = codec::decode_metrics(&body).unwrap_or_default();
                        for (series_id, value) in &decoded {
                            // v8：計數器只彙總速率
                            if !caches.is_counter(*series_id) {
                                rollup.add(
                                    host_id,
                                    *series_id,
                                    ts,
                                    value.as_f64().unwrap_or(f64::NAN),
                                    now_ms,
                                );
                            }
                        }
                        let mut state = caches.counters.remove(&host_id).unwrap_or_default();
                        let points = derive::derive(&caches.derive_items(&decoded), ts, &mut state);
                        caches.counters.insert(host_id, state);
                        for p in points {
                            // N8：這台主機已達上限時，對它而言是新的衍生序列不寫進全域 series 表
                            if !caches.host_has_room(host_id, cap)
                                && !caches.has_series_derived(&p.name, &p.unit, &p.labels, p.derived)
                            {
                                continue;
                            }
                            if let Some(id) = caches
                                .series_id_derived(&tx, &p.name, &p.unit, &p.labels, p.derived)?
                                && caches.admit(host_id, id, cap)
                            {
                                rollup.add(host_id, id, ts, p.value, now_ms);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        caches.flush_admitted(&tx)?;
        tx.commit()?;
        Ok(count)
    }

    /// 寫出已結束的 5 分鐘桶（一個交易，最多約 [`ROLLUP_CHUNK_ROWS`] 列），同時合併進 1 小時彙總，
    /// 並推進這些主機的 `rollup_until`
    fn flush_rollup(&mut self, now_ms: i64) -> PgResult<usize> {
        let rows = self.rollup.take_ready(now_ms, ROLLUP_CHUNK_ROWS);
        if rows.is_empty() {
            return Ok(0);
        }
        let Some(conn) = self.conn.as_ref() else {
            self.rollup.put_back(rows);
            return Ok(0);
        };
        let mut until: BTreeMap<i64, i64> = BTreeMap::new();
        for row in &rows {
            let end = (row.bucket + BUCKET_5M_SECS) * 1000;
            let e = until.entry(row.host_id).or_insert(end);
            *e = (*e).max(end);
        }
        let result = (|| -> PgResult<()> {
            let tx = conn.begin_tx(TxMode::Deferred)?;
            merge_rollup_rows(&tx, &rows)?;
            tx.execute(
                "UPDATE hosts h SET rollup_until = GREATEST(h.rollup_until, u.until)
                 FROM unnest(?1::bigint[], ?2::bigint[]) AS u(host_id, until)
                 WHERE h.host_id = u.host_id",
                vec![array(until.keys()), array(until.values())],
            )?;
            tx.commit()
        })();
        match result {
            Ok(()) => {
                for entry in self.caches.hosts.values_mut() {
                    if let Some(end) = until.get(&entry.host_id) {
                        entry.rollup_until = entry.rollup_until.max(*end);
                    }
                }
                Ok(rows.len())
            }
            Err(e) => {
                self.rollup.put_back(rows);
                Err(e)
            }
        }
    }

    /// 目前 schema 所有資料表（含索引與 TOAST）的大小。刪除資料後不會變小（見本檔開頭）
    fn used_bytes(&self) -> PgResult<u64> {
        let Some(conn) = self.conn.as_ref() else {
            return Ok(0);
        };
        conn.query_row(USED_BYTES_SQL, (), |r| r.get::<_, u64>(0))
    }

    /// 開始一輪容量刪除前：更新統計資訊，估計每張表每列的大小（資料表大小 ÷ 列數）
    fn estimate_row_bytes(&self) -> PgResult<[u64; 4]> {
        let mut out = [MIN_ROW_BYTES; 4];
        let Some(conn) = self.conn.as_ref() else {
            return Ok(out);
        };
        for (slot, table) in out.iter_mut().zip(CAPACITY_TABLES) {
            conn.execute_batch(&format!("ANALYZE {table}"))?;
            let (size, rows): (u64, f64) = conn.query_row(
                "SELECT pg_total_relation_size(c.oid)::bigint, c.reltuples::double precision
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = current_schema() AND c.relname = ?1",
                [table],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if rows >= 1.0 {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let per_row = (size as f64 / rows) as u64;
                *slot = per_row.max(MIN_ROW_BYTES);
            }
        }
        Ok(out)
    }

    /// 上一輪結束時的使用量（第一次使用時從 `meta` 讀取）
    fn high_water(&mut self) -> u64 {
        if let Some(v) = self.capacity.high_water {
            return v;
        }
        let saved: Option<u64> = self.conn.as_ref().and_then(|conn| {
            conn.query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [META_CAPACITY_HIGH_WATER],
                |r| r.get::<_, u64>(0),
            )
            .optional()
            .ok()
            .flatten()
        });
        let v = saved.unwrap_or(0);
        self.capacity.high_water = Some(v);
        v
    }

    /// 一輪容量刪除結束：VACUUM 讓刪出的空間可以重複使用，重新量測並記下新的門檻基準。回傳使用量
    fn finish_capacity_round(&mut self) -> u64 {
        if let Some(conn) = self.conn.as_ref() {
            // VACUUM 不能在交易（含多個敘述的批次）中執行，也可能超過寫入的敘述逾時
            let _ = conn.execute_batch("SET statement_timeout = 0");
            for table in CAPACITY_TABLES {
                if let Err(e) = conn.execute_batch(&format!("VACUUM (ANALYZE) {table}")) {
                    debug!("VACUUM {table} 失敗（空間由 autovacuum 回收）: {e}");
                }
            }
            if let Err(e) = Self::apply_timeout(conn, self.write_timeout) {
                debug!("無法恢復 statement_timeout: {e}");
            }
        }
        let used = self.used_bytes().unwrap_or(0);
        self.capacity.high_water = Some(used);
        if let Some(conn) = self.conn.as_ref()
            && let Err(e) = conn.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![META_CAPACITY_HIGH_WATER, used.to_string()],
            )
        {
            debug!("無法記錄容量刪除後的使用量: {e}");
        }
        self.capacity.active = false;
        used
    }

    /// 扣掉這一步估計刪出的空間
    fn credit(&mut self, table: usize, rows: u64) {
        let bytes = rows.saturating_mul(self.capacity.row_bytes[table]);
        self.capacity.budget = self
            .capacity
            .budget
            .saturating_sub(i64::try_from(bytes).unwrap_or(i64::MAX));
    }

    /// 總容量上限：超過門檻時刪除一批最舊的資料（規則見本檔開頭）。回傳是否還要繼續刪
    fn capacity_step(&mut self) -> PgResult<bool> {
        let cap = self.opts.max_total_bytes;
        if cap == 0 {
            return Ok(false);
        }
        if !self.capacity.active {
            let now = Instant::now();
            if self.capacity.next_check.is_some_and(|t| now < t) {
                return Ok(false);
            }
            self.capacity.next_check = Some(now + CAPACITY_CHECK_INTERVAL);
            let used = self.used_bytes()?;
            let threshold = cap.max(self.high_water().saturating_add(cap / 100));
            if used <= threshold {
                return Ok(false);
            }
            let target = cap / 100 * CAPACITY_LOW_WATERMARK_PERCENT;
            self.capacity.row_bytes = self.estimate_row_bytes()?;
            self.capacity.active = true;
            self.capacity.deleted = [0; 3];
            self.capacity.budget = i64::try_from(used.saturating_sub(target)).unwrap_or(i64::MAX);
            self.capacity.started_at = (self.clock)().timestamp_millis();
            warn!(
                "警告 {CODE_CAPACITY}：資料庫已達容量上限，正在提早刪除最舊的資料：已使用 {} bytes，超過 storage.max_total_bytes（{} bytes）；依序刪除最舊的原始 report、5 分鐘彙總、1 小時彙總，約 {} bytes（降到上限的 {}%）。PostgreSQL 刪除資料後檔案不會變小，刪出的空間由之後的資料重複使用。新的回報照常保存，實際保留期會短於設定（處理方式：依 STORAGE.md「容量估算」調高 storage.max_total_bytes 或縮短保留天數後重新啟動 Kairos）",
                used, cap, self.capacity.budget, CAPACITY_LOW_WATERMARK_PERCENT
            );
            self.save_capacity_event(used, None, false);
        } else if self.capacity.budget <= 0 {
            let used = self.finish_capacity_round();
            let [raw, m5, h1] = self.capacity.deleted;
            info!(
                "{CODE_CAPACITY} 結束：已刪除估計足夠的資料（PostgreSQL 的資料表共 {used} bytes，上限 {cap} bytes；刪出的空間會重複使用），停止刪除；這一輪刪除原始 report {raw} 筆、5 分鐘彙總 {m5} 列、1 小時彙總 {h1} 列"
            );
            self.save_capacity_event(used, Some((self.clock)().timestamp_millis()), false);
            return Ok(false);
        }
        let Some(conn) = self.conn.as_ref() else {
            return Ok(false);
        };
        let raw = conn.execute(
            "DELETE FROM reports WHERE id IN (SELECT id FROM reports ORDER BY id LIMIT ?1)",
            [DELETE_BATCH_ROWS],
        )?;
        // 行程快照與原始資料同一層：刪到目前最舊（最早寫入）的一筆原始 report 的取樣時間為止
        // （沒有原始資料時全部刪除）
        let oldest_raw: Option<i64> = conn
            .query_row("SELECT ts FROM reports ORDER BY id LIMIT 1", (), |r| {
                r.get(0)
            })
            .optional()?;
        let processes = conn.execute(
            "DELETE FROM process_snapshots WHERE (host_id, ts) IN
             (SELECT host_id, ts FROM process_snapshots WHERE ts < ?1 ORDER BY ts LIMIT ?2)",
            params![oldest_raw.unwrap_or(i64::MAX), DELETE_BATCH_ROWS],
        )?;
        if raw > 0 || processes > 0 {
            self.capacity.deleted[0] += usize_u64(raw);
            self.credit(0, usize_u64(raw));
            self.credit(3, usize_u64(processes));
            return Ok(true);
        }
        for (i, table) in [(1, "rollup_5m"), (2, "rollup_1h")] {
            let n = delete_oldest_rollup_day(conn, table)?;
            if n > 0 {
                self.capacity.deleted[i] += n;
                self.credit(i, n);
                return Ok(true);
            }
        }
        let used = self.finish_capacity_round();
        throttled!(
            self.throttle,
            Category::StorageMaintenance,
            warn,
            "警告 {}：PostgreSQL 的資料表共 {} bytes，超過 storage.max_total_bytes（{} bytes），但已沒有原始 report 或彙總可刪除（主機清冊與序列不會自動刪除）；請調高上限",
            CODE_CAPACITY,
            used,
            cap
        );
        self.save_capacity_event(used, Some((self.clock)().timestamp_millis()), true);
        Ok(false)
    }

    /// 把這一輪容量上限刪除的狀態寫入 `meta.capacity_event`（系統健康的「最近一次容量刪除」）
    fn save_capacity_event(&mut self, used: u64, ended_at: Option<i64>, exhausted: bool) {
        let [raw, m5, h1] = self.capacity.deleted;
        let event = CapacityEvent {
            started_at: self.capacity.started_at,
            ended_at,
            used_bytes: used,
            limit_bytes: self.opts.max_total_bytes,
            deleted_raw: raw,
            deleted_rollup_5m: m5,
            deleted_rollup_1h: h1,
            exhausted,
        };
        if let Some(conn) = self.conn.as_ref()
            && let Err(e) = put_meta(conn, META_CAPACITY_EVENT, &event)
        {
            debug!("無法記錄容量刪除的事件: {e}");
        }
    }

    /// 開始新一輪的保留期限刪除；日期保護判斷時鐘可能被誤校時不刪除
    fn start_retention(&mut self, now: DateTime<Utc>) -> Option<RetentionJob> {
        let opts = &self.opts;
        if opts.raw_retention_days == 0
            && opts.rollup_5m_retention_days == 0
            && opts.rollup_1h_retention_days == 0
        {
            return None;
        }
        let guard = self.guard.as_mut()?;
        let decision = guard.decide(now.date_naive(), Instant::now(), now);
        for event in &decision.events {
            log_guard_event(GUARD_LABEL, event);
        }
        let state = guard.state(Instant::now(), now);
        if let (Some(conn), Ok(bytes)) = (self.conn.as_ref(), state.to_file_bytes())
            && let Err(e) = conn.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![META_RETENTION_GUARD, String::from_utf8_lossy(&bytes)],
            )
        {
            throttled!(
                self.throttle,
                Category::StorageMaintenance,
                warn,
                "無法保存日期保護的狀態（重新啟動後的時鐘跳躍保護可能失效）: {}",
                e
            );
        }
        let date = decision.cleanup_date?;
        let before = |days: u32| -> Option<i64> {
            if days == 0 {
                return None;
            }
            let keep_from: NaiveDate = date.checked_sub_days(chrono::Days::new(u64::from(days)))?;
            Some(keep_from.and_hms_opt(0, 0, 0)?.and_utc().timestamp())
        };
        let mut hosts: Vec<i64> = self.caches.hosts.values().map(|e| e.host_id).collect();
        hosts.sort_unstable();
        Some(RetentionJob {
            stage: RetentionStage::Raw,
            hosts,
            cursor: 0,
            raw_before_ms: before(opts.raw_retention_days).map(|s| s * 1000),
            rollup_5m_before: before(opts.rollup_5m_retention_days),
            rollup_1h_before: before(opts.rollup_1h_retention_days),
            deleted: [0; 6],
        })
    }

    /// 保留期限：每小時一輪，每一步刪除最多 [`DELETE_BATCH_ROWS`] 列。回傳這一輪是否還沒做完
    fn retention_step(&mut self, now: DateTime<Utc>) -> PgResult<bool> {
        if self.retention.is_none() {
            let due = self.next_retention.is_none_or(|t| Instant::now() >= t);
            if !due {
                return Ok(false);
            }
            self.next_retention = Some(Instant::now() + RETENTION_INTERVAL);
            self.retention = self.start_retention(now);
            if self.retention.is_none() {
                return Ok(false);
            }
        }
        let Some(conn) = self.conn.as_ref() else {
            return Ok(false);
        };
        let Some(job) = self.retention.as_mut() else {
            return Ok(false);
        };
        let mut budget = DELETE_BATCH_ROWS;
        let tx = conn.begin_tx(TxMode::Deferred)?;
        while budget > 0 {
            let (sql, before) = match job.stage {
                RetentionStage::Raw => (
                    "DELETE FROM reports WHERE id IN
                     (SELECT id FROM reports WHERE host_id = ?1 AND ts < ?2 LIMIT ?3)",
                    job.raw_before_ms,
                ),
                RetentionStage::Processes => (
                    "DELETE FROM process_snapshots WHERE (host_id, ts) IN
                     (SELECT host_id, ts FROM process_snapshots WHERE host_id = ?1 AND ts < ?2 LIMIT ?3)",
                    job.raw_before_ms,
                ),
                RetentionStage::Changes => (
                    "DELETE FROM inventory_changes WHERE id IN
                     (SELECT id FROM inventory_changes WHERE host_id = ?1 AND changed_at < ?2 LIMIT ?3)",
                    job.rollup_1h_before.map(|s| s * 1000),
                ),
                RetentionStage::Rollup5m => (
                    "DELETE FROM rollup_5m WHERE (host_id, bucket, series_id) IN
                     (SELECT host_id, bucket, series_id FROM rollup_5m WHERE host_id = ?1 AND bucket < ?2 LIMIT ?3)",
                    job.rollup_5m_before,
                ),
                RetentionStage::Rollup1h => (
                    "DELETE FROM rollup_1h WHERE (host_id, bucket, series_id) IN
                     (SELECT host_id, bucket, series_id FROM rollup_1h WHERE host_id = ?1 AND bucket < ?2 LIMIT ?3)",
                    job.rollup_1h_before,
                ),
                RetentionStage::Boots => {
                    // 已沒有任何 report 引用的 boot（去重鍵的 boot_id 對照）
                    let n = usize_i64(tx.execute(
                        "DELETE FROM boots WHERE boot_ref IN
                         (SELECT boot_ref FROM boots b WHERE NOT EXISTS
                            (SELECT 1 FROM reports r WHERE r.boot_ref = b.boot_ref) LIMIT ?1)",
                        [budget],
                    )?);
                    job.deleted[3] += u64::try_from(n).unwrap_or(0);
                    if n < budget {
                        break;
                    }
                    budget -= n;
                    continue;
                }
            };
            let next_stage = |s| match s {
                RetentionStage::Raw => RetentionStage::Processes,
                RetentionStage::Processes => RetentionStage::Rollup5m,
                RetentionStage::Rollup5m => RetentionStage::Rollup1h,
                RetentionStage::Rollup1h => RetentionStage::Changes,
                _ => RetentionStage::Boots,
            };
            let (Some(before), Some(&host_id)) = (before, job.hosts.get(job.cursor)) else {
                job.stage = next_stage(job.stage);
                job.cursor = 0;
                continue;
            };
            let limit = budget;
            let n = usize_i64(tx.execute(sql, params![host_id, before, limit])?);
            let slot = match job.stage {
                RetentionStage::Raw => 0,
                RetentionStage::Rollup5m => 1,
                RetentionStage::Rollup1h => 2,
                RetentionStage::Processes => 4,
                _ => 5,
            };
            job.deleted[slot] += u64::try_from(n).unwrap_or(0);
            budget -= n;
            if n < limit {
                // 這台主機已刪完；刪滿額度時下一步從同一台繼續
                job.cursor += 1;
            }
        }
        tx.commit()?;
        let done = job.stage == RetentionStage::Boots && budget > 0;
        if done {
            let [raw, m5, h1, boots, processes, changes] = job.deleted;
            if raw + m5 + h1 + boots + processes + changes > 0 {
                info!(
                    "已依保留期限刪除 PostgreSQL 中過期的資料：原始 report {raw} 筆、5 分鐘彙總 {m5} 列、1 小時彙總 {h1} 列、行程快照 {processes} 筆、資產變更歷史 {changes} 筆（storage.raw_retention_days、rollup_5m_retention_days、rollup_1h_retention_days）"
                );
            }
            let run = RetentionRun {
                finished_at: now.timestamp_millis(),
                deleted_raw: raw,
                deleted_rollup_5m: m5,
                deleted_rollup_1h: h1,
            };
            if let Err(e) = put_meta(conn, META_RETENTION_LAST, &run) {
                debug!("無法記錄保留清除的結果: {e}");
            }
            self.retention = None;
            // boot 對照表可能刪除了快取中的項目
            self.caches.boots.clear();
        }
        Ok(!done)
    }

    /// 一步維護工作：主機清冊的下一批 → 寫出彙總 → 容量上限 → 保留期限。回傳是否還有工作
    /// （呼叫端稍後再呼叫）
    fn maintain_step(&mut self) -> PgResult<bool> {
        if !self.pending_inventory.is_empty() {
            self.sync_chunk()?;
            return Ok(true);
        }
        let now = (self.clock)();
        // 最舊的桶已結束但仍在補送（或所屬主機較早的桶還在補）時，這一步可能沒有可寫出的列，
        // 照常繼續做容量上限與保留期限
        if self.rollup.has_ready(now.timestamp_millis())
            && self.flush_rollup(now.timestamp_millis())? > 0
        {
            return Ok(true);
        }
        if self.capacity_step()? {
            return Ok(true);
        }
        self.retention_step(now)
    }

    /// 主機清冊同步：狀態表中有變化的主機寫入 `hosts`（規則與 SQLite 後端相同：`last_seen` 與
    /// `last_report` 只在 `last_seen` 相差 60 秒以上時才寫，其他欄位改變時立即寫入）。先寫入一批
    /// （[`INVENTORY_CHUNK_ROWS`] 列、一個交易），其餘由之後的維護步驟逐批寫入。回傳這次寫入的列數
    fn sync(&mut self, snapshot: &RegistrySnapshot) -> PgResult<usize> {
        let mut rows = VecDeque::new();
        let mut new_hosts = 0;
        self.expire_purging();
        for agent in &snapshot.agents {
            if self.purging.contains_key(agent.agent_id.as_str()) {
                continue;
            }
            let row = InventoryRow::of(agent, self.opts.mtls);
            let entry = self.caches.hosts.get(agent.agent_id.as_str());
            let unchanged = entry.and_then(|e| e.synced).is_some_and(|(hash, seen)| {
                hash == row.hash && row.last_seen - seen < LAST_SEEN_WRITE_INTERVAL_MS
            });
            if unchanged {
                continue;
            }
            if entry.is_none() && self.caches.hosts.len() + new_hosts >= self.opts.max_hosts {
                throttled!(
                    self.throttle,
                    Category::StorageMaintenance,
                    warn,
                    "主機清冊已達上限 {}（storage.max_hosts），agent {} 不寫入清冊",
                    self.opts.max_hosts,
                    agent.agent_id
                );
                continue;
            }
            if entry.is_none() {
                new_hosts += 1;
            }
            rows.push_back(row);
        }
        self.pending_inventory = rows;
        self.sync_chunk()
    }

    /// 寫入待寫入清冊的下一批（一個交易）；失敗時放回，下一次再試。回傳寫入的列數
    fn sync_chunk(&mut self) -> PgResult<usize> {
        if self.pending_inventory.is_empty() {
            return Ok(0);
        }
        let n = self.pending_inventory.len().min(INVENTORY_CHUNK_ROWS);
        let rows: Vec<InventoryRow> = self.pending_inventory.drain(..n).collect();
        match self.write_inventory(&rows) {
            Ok(()) => Ok(rows.len()),
            Err(e) => {
                for row in rows.into_iter().rev() {
                    self.pending_inventory.push_front(row);
                }
                Err(e)
            }
        }
    }

    /// 在一個交易中把 `rows` 寫入清冊，成功後更新快取
    fn write_inventory(&mut self, rows: &[InventoryRow]) -> PgResult<()> {
        let Some(conn) = self.conn.as_ref() else {
            return Ok(());
        };
        let mut written = Vec::with_capacity(rows.len());
        let tx = conn.begin_tx(TxMode::Deferred)?;
        for row in rows {
            let (host_id, rollup_until): (i64, i64) = tx.query_row(
                "INSERT INTO hosts (agent_id, hostname, agent_version, proto, host_fingerprint,
                    display_name, tags, os, report_interval_ms, peer, identity_verified, online,
                    first_seen, connected_at, last_seen, offline_after_ms, last_report,
                    install_method, install_upgrade, install_config)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                    ?18, ?19, ?20)
                 ON CONFLICT (agent_id) DO UPDATE SET
                    hostname = excluded.hostname,
                    agent_version = excluded.agent_version,
                    proto = excluded.proto,
                    host_fingerprint = COALESCE(excluded.host_fingerprint, hosts.host_fingerprint),
                    display_name = COALESCE(excluded.display_name, hosts.display_name),
                    tags = COALESCE(excluded.tags, hosts.tags),
                    os = COALESCE(excluded.os, hosts.os),
                    report_interval_ms = COALESCE(excluded.report_interval_ms, hosts.report_interval_ms),
                    peer = excluded.peer,
                    identity_verified = excluded.identity_verified,
                    online = excluded.online,
                    connected_at = excluded.connected_at,
                    last_seen = GREATEST(hosts.last_seen, excluded.last_seen),
                    offline_after_ms = excluded.offline_after_ms,
                    last_report = COALESCE(excluded.last_report, hosts.last_report),
                    install_method = COALESCE(excluded.install_method, hosts.install_method),
                    install_upgrade = COALESCE(excluded.install_upgrade, hosts.install_upgrade),
                    install_config = COALESCE(excluded.install_config, hosts.install_config)
                 RETURNING host_id, rollup_until",
                params![
                    row.agent_id,
                    row.hostname,
                    row.agent_version,
                    row.proto,
                    row.host_fingerprint,
                    row.display_name,
                    row.tags,
                    row.os,
                    row.report_interval_ms,
                    row.peer,
                    row.identity_verified,
                    row.online,
                    row.connected_at.unwrap_or(row.last_seen),
                    row.connected_at,
                    row.last_seen,
                    row.offline_after_ms,
                    row.last_report,
                    row.install_method,
                    row.install_upgrade,
                    row.install_config,
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            written.push((host_id, rollup_until));
        }
        tx.commit()?;
        for (row, (host_id, rollup_until)) in rows.iter().zip(written) {
            let entry = self
                .caches
                .hosts
                .entry(row.agent_id.clone())
                .or_insert(HostEntry {
                    host_id,
                    rollup_until,
                    synced: None,
                });
            entry.synced = Some((row.hash, row.last_seen));
        }
        Ok(())
    }

    /// 保存一則資產；失敗時回傳原因（連線回 `storage_unavailable`）
    fn store_assets(&mut self, w: &AssetWrite) -> Result<AssetOutcome, String> {
        self.ensure_ready()?;
        match self.assets_tx(w) {
            Ok(Some(outcome)) => Ok(outcome),
            Ok(None) => Err(HOSTS_FULL.to_string()),
            Err(e) => {
                let reason = format!("寫入 {} 的主機資產失敗: {}", self.target, e);
                throttled!(
                    self.throttle,
                    Category::StorageWrite,
                    error,
                    "{}（agent {}）",
                    reason,
                    w.agent_id
                );
                self.invalidate(e.is_connection_fatal());
                Err(reason)
            }
        }
    }

    /// 與上一次合併、摘要改變時寫入變更歷史、更新開機時間（一個交易）
    fn assets_tx(&mut self, w: &AssetWrite) -> PgResult<Option<AssetOutcome>> {
        let Self {
            conn, caches, opts, ..
        } = self;
        let Some(conn) = conn.as_ref() else {
            return Err(PgError::other("資料庫未連線"));
        };
        let tx = conn.begin_tx(TxMode::Deferred)?;
        let Some(host_id) =
            caches.host_id_of(&tx, &w.agent_id, &w.hostname, &w.peer, w.received_ms, opts)?
        else {
            return Ok(None);
        };
        let prev: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT digest, body, changed_at FROM host_inventory WHERE host_id = ?1",
                [host_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let prev_inv: Option<Inventory> = prev
            .as_ref()
            .and_then(|(_, body, _)| serde_json::from_str(body).ok());
        let merged = assets::merge(prev_inv.as_ref(), &w.inventory);
        let digest = inventory_digest(&merged);
        let first = prev.is_none();
        let changed = prev.as_ref().is_some_and(|(d, _, _)| *d != digest);
        let mut history = 0;
        if changed && let Some(before) = &prev_inv {
            let changes: Vec<Change> = assets::diff(before, &merged);
            if !changes.is_empty() {
                tx.execute(
                    "INSERT INTO inventory_changes (host_id, changed_at, changes) VALUES (?1, ?2, ?3)",
                    params![
                        host_id,
                        w.collected_ms,
                        serde_json::to_string(&changes).unwrap_or_else(|_| "[]".into())
                    ],
                )?;
                tx.execute(
                    "DELETE FROM inventory_changes WHERE host_id = ?1 AND id NOT IN
                     (SELECT id FROM inventory_changes WHERE host_id = ?1 ORDER BY id DESC LIMIT ?2)",
                    params![host_id, MAX_CHANGES_PER_HOST],
                )?;
                history = 1;
            }
        }
        let changed_at = match &prev {
            Some((_, _, at)) if !changed => *at,
            _ => w.collected_ms,
        };
        let os = merged.os.as_ref();
        let cpu = merged.cpu.as_ref();
        let sys = merged.system.as_ref();
        let logical_cores = cpu.and_then(|c| c.logical_cores);
        let os_family = os.and_then(|o| o.family.clone());
        tx.execute(
            "INSERT INTO host_inventory (host_id, digest, body, collected_at, received_at, changed_at,
                os_family, cpu_model, cpu_logical_cores, memory_total, vendor, product, serial)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT (host_id) DO UPDATE SET
                digest = excluded.digest, body = excluded.body, collected_at = excluded.collected_at,
                received_at = excluded.received_at, changed_at = excluded.changed_at,
                os_family = excluded.os_family, cpu_model = excluded.cpu_model,
                cpu_logical_cores = excluded.cpu_logical_cores, memory_total = excluded.memory_total,
                vendor = excluded.vendor, product = excluded.product, serial = excluded.serial",
            params![
                host_id,
                digest,
                serde_json::to_string(&merged).unwrap_or_else(|_| "{}".into()),
                w.collected_ms,
                w.received_ms,
                changed_at,
                os_family,
                cpu.and_then(|c| c.model.clone()),
                logical_cores,
                merged
                    .memory
                    .as_ref()
                    .and_then(|m| m.total)
                    .and_then(|t| i64::try_from(t).ok()),
                sys.and_then(|s| s.vendor.clone()),
                sys.and_then(|s| s.product.clone()),
                sys.and_then(|s| s.serial.clone()),
            ],
        )?;
        if let Some(boot) = assets::boot_time_secs(&w.inventory) {
            tx.execute(
                "UPDATE hosts SET boot_time = ?2 WHERE host_id = ?1",
                params![host_id, boot],
            )?;
        }
        tx.commit()?;
        if first {
            debug!("首次收到主機 {} 的資產（{}）", w.agent_id, digest);
        } else if changed {
            debug!(
                "主機 {} 的資產已改變（{}；變更歷史 {} 筆）",
                w.agent_id, digest, history
            );
        }
        Ok(Some(AssetOutcome {
            first,
            changed,
            history,
            logical_cores,
            os_family,
        }))
    }

    /// 刪除主機資料：做一步；回傳是否完成。失敗時回傳原因（已完成的步驟保留，重新執行即可接續）。
    /// 第一步清除這台主機的快取與記憶體中尚未寫出的彙總；執行期間與完成後 [`PURGE_HOLD`] 內不寫入
    /// 它的 report、資產與清冊同步，避免被建立回來
    fn purge(&mut self, job: &mut PurgeJob) -> Result<bool, String> {
        self.ensure_ready()?;
        if !job.started {
            self.purging.insert(job.agent_id.clone(), Instant::now());
            let host_id = match self.caches.hosts.remove(&job.agent_id) {
                Some(e) => Some(e.host_id),
                None => {
                    let conn = self.conn.as_ref().ok_or("資料庫未連線")?;
                    super::purge::host_id_of(conn, &job.agent_id).map_err(|e| e.to_string())?
                }
            };
            if let Some(h) = host_id {
                self.caches.host_series.remove(&h);
                self.caches.counters.remove(&h);
                self.caches.seeded.remove(&h);
                self.caches.boots.remove(&h);
                job.counts.rollup_pending += usize_u64(self.rollup.remove_host(h));
            }
            self.pending_inventory
                .retain(|r| r.agent_id != job.agent_id);
            self.series_cap_logged.remove(&job.agent_id);
        }
        let now_ms = (self.clock)().timestamp_millis();
        let Some(conn) = self.conn.as_ref() else {
            return Err("資料庫未連線".into());
        };
        match super::purge::step(conn, job, DELETE_BATCH_ROWS, now_ms) {
            Ok(done) => {
                if done {
                    // 保留清除的進度中可能有這台主機（刪除不存在的主機的資料沒有影響），boot 快取清空
                    self.caches.boots.clear();
                }
                Ok(done)
            }
            Err(e) => {
                let reason = format!(
                    "刪除主機 {} 的資料時資料庫錯誤（已完成的部分保留，重新執行可接續）: {e}",
                    job.agent_id
                );
                self.invalidate(e.is_connection_fatal());
                Err(reason)
            }
        }
    }
}

/// 把 5 分鐘桶的列合併進 `rollup_5m` 與 `rollup_1h`（已存在的列累加 min／max／sum／count，
/// 不覆蓋）。寫出記憶體中的桶、以及遲到資料直接合併進已寫出的桶時共用
pub(crate) fn merge_rollup_rows(tx: &PgConn, rows: &[RollupRow]) -> PgResult<()> {
    merge_rollup_rows_from(tx, rows, None)
}

/// [`merge_rollup_rows`]，但 `bucket`（秒）早於 `rollup_5m_from` 的列只合併進 `rollup_1h`
pub(crate) fn merge_rollup_rows_from(
    tx: &PgConn,
    rows: &[RollupRow],
    rollup_5m_from: Option<i64>,
) -> PgResult<()> {
    let m5: Vec<(i64, i64, i64, Agg)> = rows
        .iter()
        .filter(|r| rollup_5m_from.is_none_or(|from| r.bucket >= from))
        .map(|r| (r.host_id, r.bucket, r.series_id, r.agg))
        .collect();
    upsert_rollup(tx, "rollup_5m", m5)?;
    let h1: Vec<(i64, i64, i64, Agg)> = rows
        .iter()
        .map(|r| {
            (
                r.host_id,
                bucket_start(r.bucket * 1000, BUCKET_1H_SECS),
                r.series_id,
                r.agg,
            )
        })
        .collect();
    upsert_rollup(tx, "rollup_1h", h1)
}

/// 以多列的 upsert 合併進彙總表。同一個敘述不能兩次更新同一列（`ON CONFLICT DO UPDATE` 的限制），而
/// 同一個小時有多個 5 分鐘桶：分成數輪，第 k 輪放每個鍵的第 k 次出現。每個鍵的套用順序與逐列 upsert
/// 相同，浮點數的累加結果因此與 SQLite 後端一致
fn upsert_rollup(tx: &PgConn, table: &str, rows: Vec<(i64, i64, i64, Agg)>) -> PgResult<()> {
    let sql = format!(
        "INSERT INTO {table} (host_id, bucket, series_id, vmin, vmax, vsum, cnt)
         SELECT * FROM unnest(?1::bigint[], ?2::bigint[], ?3::bigint[], ?4::double precision[],
            ?5::double precision[], ?6::double precision[], ?7::bigint[])
         ON CONFLICT (host_id, bucket, series_id) DO UPDATE SET
           vmin = LEAST({table}.vmin, excluded.vmin), vmax = GREATEST({table}.vmax, excluded.vmax),
           vsum = {table}.vsum + excluded.vsum, cnt = {table}.cnt + excluded.cnt"
    );
    let mut passes: Vec<Vec<(i64, i64, i64, Agg)>> = Vec::new();
    let mut seen: HashMap<(i64, i64, i64), usize> = HashMap::new();
    for row in rows {
        let n = seen.entry((row.0, row.1, row.2)).or_insert(0);
        if passes.len() <= *n {
            passes.push(Vec::new());
        }
        passes[*n].push(row);
        *n += 1;
    }
    for pass in passes {
        tx.execute(
            &sql,
            vec![
                array(pass.iter().map(|r| r.0)),
                array(pass.iter().map(|r| r.1)),
                array(pass.iter().map(|r| r.2)),
                array(pass.iter().map(|r| r.3.min)),
                array(pass.iter().map(|r| r.3.max)),
                array(pass.iter().map(|r| r.3.sum)),
                array(
                    pass.iter()
                        .map(|r| i64::try_from(r.3.count).unwrap_or(i64::MAX)),
                ),
            ],
        )?;
    }
    Ok(())
}

/// 刪除某個彙總表中最舊一天的資料（每台主機各自以主鍵範圍刪除），回傳刪除的列數
fn delete_oldest_rollup_day(conn: &PgConn, table: &str) -> PgResult<u64> {
    let oldest: Option<i64> = conn.query_row(
        &format!(
            "SELECT min(b) FROM (SELECT (SELECT min(bucket) FROM {table} WHERE host_id = h.host_id) AS b FROM hosts h) AS t"
        ),
        (),
        |r| r.get(0),
    )?;
    let Some(oldest) = oldest else {
        return Ok(0);
    };
    let before = bucket_start(oldest * 1000, 86_400) + 86_400;
    let n = conn.execute(
        &format!(
            "DELETE FROM {table} WHERE (host_id, bucket, series_id) IN
             (SELECT host_id, bucket, series_id FROM {table} WHERE host_id IN (SELECT host_id FROM hosts) AND bucket < ?1 LIMIT ?2)"
        ),
        params![before, DELETE_BATCH_ROWS],
    )?;
    Ok(usize_u64(n))
}

/// 清冊中 `last_seen >= since_ms` 的主機，轉成狀態表的格式
fn load_restored(conn: &PgConn, since_ms: i64) -> PgResult<Vec<RestoredAgent>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, hostname, agent_version, proto, host_fingerprint, display_name, tags, os,
                report_interval_ms, peer, connected_at, last_seen, offline_after_ms, last_report
         FROM hosts WHERE last_seen >= ?1",
    )?;
    let rows = stmt.query_map([since_ms], |r| {
        let mut host = serde_json::Map::new();
        let mut put = |key: &str, v: Option<serde_json::Value>| {
            if let Some(v) = v {
                host.insert(key.to_string(), v);
            }
        };
        put(
            "host_fingerprint",
            r.get::<_, Option<String>>(4)?.map(Into::into),
        );
        put(
            "display_name",
            r.get::<_, Option<String>>(5)?.map(Into::into),
        );
        let parse = |s: Option<String>| s.and_then(|s| serde_json::from_str(&s).ok());
        put("tags", parse(r.get(6)?));
        put("os", parse(r.get(7)?));
        put(
            "report_interval_ms",
            r.get::<_, Option<i64>>(8)?.map(Into::into),
        );
        let host: Option<HostInfo> = (!host.is_empty())
            .then(|| serde_json::from_value(serde_json::Value::Object(host)).ok())
            .flatten();
        let time = |ms: Option<i64>| ms.and_then(DateTime::from_timestamp_millis);
        let last_seen = time(r.get(11)?).unwrap_or_default();
        Ok(RestoredAgent {
            agent_id: r.get(0)?,
            hostname: r.get(1)?,
            agent_version: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            proto: r
                .get::<_, Option<i64>>(3)?
                .and_then(|p| u32::try_from(p).ok())
                .unwrap_or(1),
            host,
            peer: r.get::<_, Option<String>>(9)?.unwrap_or_default(),
            connected_at: time(r.get(10)?).unwrap_or(last_seen),
            last_seen,
            offline_after: Duration::from_millis(
                r.get::<_, Option<i64>>(12)?
                    .and_then(|v| u64::try_from(v).ok())
                    .unwrap_or(kairos_proto::MIN_OFFLINE_AFTER_MS),
            ),
            last_report: r
                .get::<_, Option<String>>(13)?
                .and_then(|s| serde_json::from_str::<LastReport>(&s).ok()),
        })
    })?;
    rows.collect()
}

impl ReportStore for PgStore {
    fn write_batch(&mut self, records: &[ReportRecord]) -> Vec<WriteStatus> {
        self.write(records)
    }

    fn write_assets(&mut self, write: &AssetWrite) -> Result<AssetOutcome, String> {
        if self.purging.contains_key(&write.agent_id) {
            return Err(PURGING.to_string());
        }
        self.store_assets(write)
    }

    fn purge_step(&mut self, job: &mut PurgeJob) -> Result<bool, String> {
        self.purge(job)
    }

    fn sync_inventory(&mut self, snapshot: &RegistrySnapshot) {
        if let Err(reason) = self.ensure_ready() {
            throttled!(
                self.throttle,
                Category::StorageMaintenance,
                warn,
                "{}，主機清冊未同步",
                reason
            );
            return;
        }
        match self.sync(snapshot) {
            Ok(n) => debug!(
                "主機清冊已同步 {} 台主機（尚有 {} 台在之後的維護步驟寫入）",
                n,
                self.pending_inventory.len()
            ),
            Err(e) => {
                throttled!(
                    self.throttle,
                    Category::StorageMaintenance,
                    warn,
                    "主機清冊同步失敗（下次同步時重試）: {}",
                    e
                );
                self.invalidate(e.is_connection_fatal());
            }
        }
    }

    fn maintain(&mut self) -> bool {
        if self.ensure_ready().is_err() {
            return false;
        }
        match self.maintain_step() {
            Ok(more) => more,
            Err(e) => {
                throttled!(
                    self.throttle,
                    Category::StorageMaintenance,
                    warn,
                    "PostgreSQL 維護工作（彙總寫出、保留期限或容量上限的刪除）失敗，稍後重試: {}",
                    e
                );
                self.invalidate(e.is_connection_fatal());
                false
            }
        }
    }

    fn close(&mut self) {
        // 清冊中尚未寫入的列全部寫完（關機時的快照：所有主機已離線）
        while !self.pending_inventory.is_empty() {
            if let Err(e) = self.sync_chunk() {
                warn!(
                    "關機前寫入主機清冊失敗，{} 台主機的最新狀態未寫入: {}",
                    self.pending_inventory.len(),
                    e
                );
                break;
            }
        }
        // 剩下的彙總留在原始資料中，下次啟動時補回；關閉連線（釋放「服務使用中」的鎖）
        self.conn = None;
    }
}

// ---- 唯讀查詢（`kairos hosts list`、`kairos reports export`）----

/// 兩個後端共用的測試（`store/backend_tests.rs`）需要的內部狀態
#[cfg(test)]
impl crate::store::backend_tests::TestStore for PgStore {
    fn set_fail(&mut self, f: FailPoint) {
        self.fail_next = Some(f);
    }
    fn rollup_len(&self) -> usize {
        self.rollup.len()
    }
    fn rollup_clear(&mut self) {
        self.rollup.clear();
    }
    fn rollup_take_count(&mut self) -> i64 {
        self.rollup
            .take_ready(i64::MAX, usize::MAX)
            .iter()
            .map(|r| r.agg.count as i64)
            .sum()
    }
    fn defer_capacity(&mut self) {
        self.capacity.next_check = Some(Instant::now() + Duration::from_secs(3600));
    }
    fn reset_capacity(&mut self) {
        self.capacity = PgCapacity::default();
    }
    fn reset_retention(&mut self) {
        self.next_retention = None;
    }
    fn set_max_total_bytes(&mut self, n: u64) {
        self.opts.max_total_bytes = n;
    }
    fn set_max_hosts(&mut self, n: usize) {
        self.opts.max_hosts = n;
    }
    fn used(&mut self) -> u64 {
        self.ensure_ready().unwrap();
        self.used_bytes().unwrap()
    }
    fn sync_now(&mut self, snapshot: &RegistrySnapshot) -> usize {
        self.sync(snapshot).unwrap()
    }
}
