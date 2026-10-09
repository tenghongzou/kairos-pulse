//! 依日期命名的檔案（`<prefix>YYYY-MM-DD<suffix>`）的檔名規則與保留清除，以及清除前的日期合理性檢查。
//!
//! reports（`reports-YYYY-MM-DD.jsonl`）與日誌（`kairos-YYYY-MM-DD.log`）共用；日期一律為 UTC。

use chrono::{DateTime, Days, NaiveDate, Utc};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{Error, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::fs;

/// 一天
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// 跳躍暫停中，日期比「跳躍時看到的日期」再往前（變晚）超過這麼多天時，重新起算暫停時間
pub const JUMP_RESTART_DAYS: u64 = 2;

/// 偵測到時鐘往前跳後，新的日期持續這麼久才接受並恢復清除
pub const JUMP_ACCEPT_AFTER: Duration = DAY;

/// 日期回撥（早於已接受的日期）持續這麼久後，把已接受的日期降回目前日期
pub const ROLLBACK_ACCEPT_AFTER: Duration = DAY;

/// 跨重新啟動時，基準點的停機時間與回撥計時的延續上限
pub const MAX_CARRIED: Duration = DAY;

/// 跨重新啟動時，跳躍暫停把停機時間算入的上限（R2-K5）。遠小於 [`JUMP_ACCEPT_AFTER`]：
/// 停機時把時鐘調快再啟動，不會因停機的牆鐘差而立即被接受；暫停時間主要靠執行中累積
pub const MAX_JUMP_DOWNTIME_CARRY: Duration = Duration::from_secs(60 * 60);

/// report 的 [`DateGuard`] 狀態檔（放在 `storage.data_dir`）。
/// 檔名不符合任何 `<prefix>YYYY-MM-DD<suffix>`，不會被清除。
pub const GUARD_STATE_FILE: &str = ".retention-guard.json";

/// 日誌的 [`DateGuard`] 狀態檔（放在 `logs/`）。與 report 的檔名不同，
/// `storage.data_dir` 設為日誌目錄時兩者也不會共用狀態（R2-K18）。
/// 舊版放在 `logs/` 的 [`GUARD_STATE_FILE`] 只在新檔不存在時讀取一次。
pub const LOG_GUARD_STATE_FILE: &str = ".retention-guard-logs.json";

/// 狀態檔內容。舊版只有 `accepted` 與 `jump_since_unix`，其餘欄位缺少時視為未設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GuardState {
    /// 最近一次接受的日期（YYYY-MM-DD）
    accepted: String,
    /// 日期基準點的日期，與存檔當時自基準點以來經過的時間（秒）
    #[serde(default)]
    anchor_date: Option<String>,
    #[serde(default)]
    anchor_elapsed_secs: Option<u64>,
    /// 存檔時的牆鐘時間（Unix 秒）；重新啟動時以「現在 − 此值」延續基準點的經過時間
    #[serde(default)]
    saved_unix: Option<i64>,
    /// 偵測到時鐘跳躍的牆鐘時間（Unix 秒，只供診斷）與當時的日期；沒有跳躍時為 `null`
    #[serde(default)]
    jump_since_unix: Option<i64>,
    #[serde(default)]
    jump_date: Option<String>,
    /// 存檔時跳躍暫停已累積的時間（秒）；重新啟動時再加上 min(max(0, 現在 − saved_unix), 1 小時)
    #[serde(default)]
    jump_elapsed_secs: Option<u64>,
    /// 偵測到日期回撥的牆鐘時間（Unix 秒）；沒有回撥時為 `null`
    #[serde(default)]
    rollback_since_unix: Option<i64>,
}

/// 計時：本次執行以單調時鐘計算，跨重新啟動以牆鐘差延續
#[derive(Debug, Clone, Copy)]
struct Timer {
    /// 本次執行中開始（或延續）計時的時間（單調時鐘）
    since: Instant,
    /// 先前的執行已經過的時間
    carried: Duration,
    /// 開始計時的牆鐘時間（寫入狀態檔用）
    since_wall: DateTime<Utc>,
}

impl Timer {
    fn start(now: Instant, now_wall: DateTime<Utc>) -> Self {
        Self {
            since: now,
            carried: Duration::ZERO,
            since_wall: now_wall,
        }
    }

    fn elapsed(&self, now: Instant) -> Duration {
        self.carried
            .saturating_add(now.saturating_duration_since(self.since))
    }

    /// 從狀態檔延續：經過時間 = 牆鐘差（`cap` 為上限）。牆鐘差為負（起點在未來）時改為從現在
    /// 重新計時，回傳的 `bool` 為 `true` 表示起點已改寫、需要存檔（R2-K7）。
    fn resume(
        since_wall: DateTime<Utc>,
        now: Instant,
        now_wall: DateTime<Utc>,
        cap: Duration,
    ) -> (Self, bool) {
        match (now_wall - since_wall).to_std() {
            Ok(diff) => (
                Self {
                    since: now,
                    carried: diff.min(cap),
                    since_wall,
                },
                false,
            ),
            Err(_) => (Self::start(now, now_wall), true),
        }
    }
}

/// 偵測中的時鐘跳躍
#[derive(Debug, Clone, Copy)]
struct Jump {
    timer: Timer,
    /// 跳躍時看到的日期（舊版狀態檔沒有這個欄位時，在下一次檢查時補上）
    date: Option<NaiveDate>,
}

/// 日期基準點：此時的日期可信。`timer` 只用來計算自基準點以來經過的時間
#[derive(Debug, Clone, Copy)]
struct Anchor {
    date: NaiveDate,
    timer: Timer,
}

impl Anchor {
    /// 依經過時間推算、目前最晚可接受的日期：`基準日期 + ceil(經過時間 / 1 天) + 1 天`
    fn allowed_max(&self, now: Instant) -> NaiveDate {
        let elapsed = self.timer.elapsed(now).as_nanos();
        let days = elapsed.div_ceil(DAY.as_nanos());
        u64::try_from(days)
            .ok()
            .and_then(|d| d.checked_add(1))
            .and_then(|d| self.date.checked_add_days(Days::new(d)))
            .unwrap_or(NaiveDate::MAX)
    }
}

/// 保留清除前的日期合理性檢查（R2-K4～K7）。
///
/// 系統時鐘被誤校到未來時，以「今天」計算的保留期限會把所有檔案判定為過期而刪光。
/// 規則（Pulse 的日誌清除採用相同規則）：
///
/// 狀態（原子寫入狀態檔，重新啟動時讀回）：
/// - `accepted`：最近一次接受的日期（清除以接受的日期為準）。
/// - 基準點 `anchor = (anchor_date, anchor_time)`：此時的日期可信。只在啟動時沒有狀態、
///   跳躍暫停被接受、回撥被接受時設定；正常推進時**不**更新。
/// - 跳躍暫停：起點牆鐘時間 `jump_since`、跳躍時看到的日期 `jump_date`。
/// - 回撥計時：起點牆鐘時間 `rollback_since`。
///
/// 經過時間：同一次執行以單調時鐘計；跨重新啟動以狀態檔記錄的牆鐘時間與現在的牆鐘差延續，
/// 負值視為 0、上限 24 小時。基準點的經過時間存成「存檔時已經過的時間 + 存檔的牆鐘時間」
/// （每次檢查都存檔），重新啟動時 = 已經過的時間 + min(max(0, 現在 − 存檔時間), 24 小時)；
/// 執行中偵測到的跳躍在存檔時已反映在存檔的牆鐘時間裡，不會在重新啟動後被當成經過時間。
/// 副作用：停機超過約 2 天後重新啟動，會暫停清除 24 小時並記 WARN（與第一輪「超過 2 天就暫停」一致）。
///
/// 每次檢查（`today` 為系統時鐘的 UTC 日期）：
/// 1. `today < accepted`（回撥）：取消跳躍暫停；回撥計時未開始則開始。回撥持續 24 小時後
///    `accepted = today`、`anchor = (today, 現在)`。以 `today` 清除（較早的日期只會刪得更少）。
/// 2. `today == accepted`：取消跳躍暫停與回撥計時，以 `today` 清除。
/// 3. `today > accepted`：取消回撥計時。允許的最晚日期
///    `allowed_max = anchor_date + ceil(自 anchor 以來經過的時間 / 1 天) + 1 天`。
///    - `today <= allowed_max`：取消跳躍暫停，`accepted = today`，以 `today` 清除。
///    - 否則（跳躍）：不清除。暫停未開始則開始（`jump_since = 現在`、`jump_date = today`）；
///      暫停中若 `today > jump_date + 2 天`，重新起算（`jump_since = 現在`、`jump_date = today`）；
///      否則暫停持續 24 小時後接受：`accepted = today`、`anchor = (today, 現在)`，以 `today` 清除。
///
/// 跨重新啟動延續跳躍暫停：狀態檔記錄「存檔時暫停已累積的時間」（每次檢查都存檔），
/// 重新啟動時 = 已累積的時間 + min(max(0, 現在 − 存檔時間), 1 小時)（R2-K5）。
/// 回撥計時跨重新啟動延續量為牆鐘差（現在 − 回撥起點）、上限 24 小時；牆鐘差為負（起點在未來）
/// 時把起點改寫為現在並存檔。存檔時間在未來時，停機期間的經過時間視為 0（下次存檔即改寫）。
///
/// 因此逐步把時鐘往前推（每次 2 天以內）也無法讓 `accepted` 超過實際經過的天數 + 1 天（R2-K4）；
/// 與重新啟動同時發生的時鐘跳躍，最多只能被基準點當成 24 小時的經過時間、被跳躍暫停當成 1 小時。
#[derive(Debug)]
pub struct DateGuard {
    /// 日誌中稱呼這類檔案的名稱
    label: &'static str,
    /// 最近一次接受的日期
    accepted: NaiveDate,
    anchor: Anchor,
    /// 偵測中的跳躍
    jump: Option<Jump>,
    /// 偵測中的日期回撥
    rollback: Option<Timer>,
    /// 狀態檔路徑；`None` 表示不保存
    state_path: Option<PathBuf>,
    /// 尚未寫入狀態檔的變更
    dirty: bool,
    /// 上次寫入狀態檔是否失敗（只在狀態改變時記錄，避免洗版）
    save_failed: bool,
}

impl DateGuard {
    /// 不保存狀態的 DateGuard，以 `baseline` 與現在為基準點
    pub fn new(
        label: &'static str,
        baseline: NaiveDate,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Self {
        Self {
            label,
            accepted: baseline,
            anchor: Anchor {
                date: baseline,
                timer: Timer::start(now, now_wall),
            },
            jump: None,
            rollback: None,
            state_path: None,
            dirty: false,
            save_failed: false,
        }
    }

    /// 從狀態檔載入（不存在時改讀 `legacy_path`）；都不存在或損毀時以 `fallback` 與現在為基準點
    /// （並在第一次檢查時寫出狀態檔）。同步讀檔，只在啟動時呼叫一次。
    pub fn load_or_new(
        label: &'static str,
        state_path: PathBuf,
        legacy_path: Option<&Path>,
        fallback: NaiveDate,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Self {
        let mut guard = Self::new(label, fallback, now, now_wall);
        let (bytes, from) = match std::fs::read(&state_path) {
            Ok(bytes) => (Some(bytes), state_path.as_path()),
            Err(_) => match legacy_path.map(|p| (std::fs::read(p), p)) {
                Some((Ok(bytes), p)) => (Some(bytes), p),
                _ => (None, state_path.as_path()),
            },
        };
        match bytes {
            Some(bytes) => match parse_state(&bytes, now, now_wall) {
                Some(parsed) => {
                    guard.accepted = parsed.accepted;
                    guard.anchor = parsed.anchor;
                    guard.jump = parsed.jump;
                    guard.rollback = parsed.rollback;
                    guard.dirty = parsed.rewritten || from != state_path.as_path();
                }
                None => {
                    warn!(
                        "{} 內容無效，改以 {} 作為清除過期的{}的日期基準",
                        from.display(),
                        fallback,
                        label
                    );
                    guard.dirty = true;
                }
            },
            None => guard.dirty = true,
        }
        guard.state_path = Some(state_path);
        guard
    }

    /// 以時鐘的「今天」判斷能否清除；可以時回傳用來清除的日期。
    /// 狀態改變時在 blocking 執行緒寫入狀態檔（含 fsync，不佔用 async worker；R2-K18）。
    /// 每次檢查都存檔（正常為每小時一次），讓狀態檔中基準點的經過時間保持最新。
    pub async fn check(
        &mut self,
        today: NaiveDate,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Option<NaiveDate> {
        let result = self.decide(today, now, now_wall);
        if self.state_path.is_some() {
            self.save(now, now_wall).await;
        }
        result
    }

    /// 規則見型別說明；只更新記憶體中的狀態
    fn decide(
        &mut self,
        today: NaiveDate,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Option<NaiveDate> {
        // 1. 日期回撥
        if today < self.accepted {
            if self.jump.take().is_some() {
                info!("系統日期已回到 {}，恢復清除過期的{}", today, self.label);
                self.dirty = true;
            }
            match self.rollback {
                None => {
                    info!(
                        "系統日期 {} 早於先前的 {}（時鐘回撥）；以較早的日期清除過期的{}，持續 {} 小時後改以此日期為基準",
                        today,
                        self.accepted,
                        self.label,
                        ROLLBACK_ACCEPT_AFTER.as_secs() / 3600
                    );
                    self.rollback = Some(Timer::start(now, now_wall));
                    self.dirty = true;
                }
                Some(timer) if timer.elapsed(now) >= ROLLBACK_ACCEPT_AFTER => {
                    info!(
                        "系統日期回撥至 {} 已持續 {} 小時，改以此日期作為清除過期的{}的基準",
                        today,
                        ROLLBACK_ACCEPT_AFTER.as_secs() / 3600,
                        self.label
                    );
                    self.accept_as_anchor(today, now, now_wall);
                }
                Some(_) => {}
            }
            return Some(today);
        }
        if self.rollback.take().is_some() {
            self.dirty = true;
        }

        // 2. 日期未變
        if today == self.accepted {
            if self.jump.take().is_some() {
                info!("系統日期已回到 {}，恢復清除過期的{}", today, self.label);
                self.dirty = true;
            }
            return Some(today);
        }

        // 3. 日期往前推進
        let allowed_max = self.anchor.allowed_max(now);
        if today <= allowed_max {
            if self.jump.take().is_some() {
                info!("系統日期已回到 {}，恢復清除過期的{}", today, self.label);
            }
            self.accepted = today;
            self.dirty = true;
            return Some(today);
        }
        match &mut self.jump {
            None => {
                warn!(
                    "系統日期 {} 晚於依經過時間推算的最晚日期 {}（先前為 {}），可能是時鐘被誤校；暫停清除過期的{}以免誤刪。若日期正確，{} 小時後會恢復清除",
                    today,
                    allowed_max,
                    self.accepted,
                    self.label,
                    JUMP_ACCEPT_AFTER.as_secs() / 3600
                );
                self.jump = Some(Jump {
                    timer: Timer::start(now, now_wall),
                    date: Some(today),
                });
                self.dirty = true;
                None
            }
            Some(jump) => {
                let jump_date = match jump.date {
                    Some(d) => d,
                    None => {
                        jump.date = Some(today);
                        self.dirty = true;
                        today
                    }
                };
                let restart_after = jump_date
                    .checked_add_days(Days::new(JUMP_RESTART_DAYS))
                    .unwrap_or(NaiveDate::MAX);
                if today > restart_after {
                    warn!(
                        "暫停清除期間系統日期再往前跳到 {}（偵測到跳躍時為 {}），重新起算 {} 小時",
                        today,
                        jump_date,
                        JUMP_ACCEPT_AFTER.as_secs() / 3600
                    );
                    *jump = Jump {
                        timer: Timer::start(now, now_wall),
                        date: Some(today),
                    };
                    self.dirty = true;
                    None
                } else if jump.timer.elapsed(now) >= JUMP_ACCEPT_AFTER {
                    warn!(
                        "系統日期 {} 已持續 {} 小時，接受為正確日期並恢復清除過期的{}",
                        today,
                        JUMP_ACCEPT_AFTER.as_secs() / 3600,
                        self.label
                    );
                    self.accept_as_anchor(today, now, now_wall);
                    Some(today)
                } else {
                    None
                }
            }
        }
    }

    /// 接受 `today` 並以現在為新的基準點（跳躍暫停或回撥被接受時）
    fn accept_as_anchor(&mut self, today: NaiveDate, now: Instant, now_wall: DateTime<Utc>) {
        self.accepted = today;
        self.anchor = Anchor {
            date: today,
            timer: Timer::start(now, now_wall),
        };
        self.jump = None;
        self.rollback = None;
        self.dirty = true;
    }

    fn state(&self, now: Instant, now_wall: DateTime<Utc>) -> GuardState {
        let fmt = |d: NaiveDate| d.format("%Y-%m-%d").to_string();
        GuardState {
            accepted: fmt(self.accepted),
            anchor_date: Some(fmt(self.anchor.date)),
            anchor_elapsed_secs: Some(self.anchor.timer.elapsed(now).as_secs()),
            saved_unix: Some(now_wall.timestamp()),
            jump_since_unix: self.jump.map(|j| j.timer.since_wall.timestamp()),
            jump_date: self.jump.and_then(|j| j.date).map(fmt),
            jump_elapsed_secs: self.jump.map(|j| j.timer.elapsed(now).as_secs()),
            rollback_since_unix: self.rollback.map(|t| t.since_wall.timestamp()),
        }
    }

    /// 原子寫入狀態檔（暫存檔同步到磁碟後 rename）
    async fn save(&mut self, now: Instant, now_wall: DateTime<Utc>) {
        let Some(path) = self.state_path.clone() else {
            self.dirty = false;
            return;
        };
        let state = self.state(now, now_wall);
        let target = path.clone();
        let result = tokio::task::spawn_blocking(move || {
            let json = serde_json::to_vec_pretty(&state).map_err(Error::other)?;
            if let Some(dir) = target.parent()
                && !dir.as_os_str().is_empty()
            {
                std::fs::create_dir_all(dir)?;
            }
            write_atomically(&target, &[&json, b"\n"])
        })
        .await
        .unwrap_or_else(|e| Err(Error::other(e)));
        match result {
            Ok(()) => {
                if self.save_failed {
                    info!("{} 已恢復寫入", path.display());
                }
                self.save_failed = false;
                self.dirty = false;
            }
            Err(e) => {
                if !self.save_failed {
                    warn!(
                        "無法寫入 {}（重新啟動後的時鐘跳躍保護可能失效）: {}",
                        path.display(),
                        e
                    );
                }
                self.save_failed = true;
            }
        }
    }
}

/// 狀態檔解析結果
struct Parsed {
    accepted: NaiveDate,
    anchor: Anchor,
    jump: Option<Jump>,
    rollback: Option<Timer>,
    /// 載入時改寫了未來的起點，或補上舊版缺少的欄位：需要存檔
    rewritten: bool,
}

/// 解析狀態檔；格式不符時回傳 `None`
fn parse_state(bytes: &[u8], now: Instant, now_wall: DateTime<Utc>) -> Option<Parsed> {
    let state: GuardState = serde_json::from_slice(bytes).ok()?;
    let date = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok();
    let wall = |unix: i64| DateTime::<Utc>::from_timestamp(unix, 0);
    let accepted = date(&state.accepted)?;
    let mut rewritten = false;
    // 停機期間的牆鐘差（現在 − 存檔時間）；存檔時間在未來時視為 0、沒有存檔時間（舊版）時為 0
    let downtime = match state.saved_unix {
        None => Duration::ZERO,
        Some(saved) => match (now_wall - wall(saved)?).to_std() {
            Ok(gap) => gap,
            Err(_) => {
                rewritten = true;
                Duration::ZERO
            }
        },
    };

    let anchor = match (
        state.anchor_date,
        state.anchor_elapsed_secs,
        state.saved_unix,
    ) {
        (Some(d), Some(elapsed), Some(_)) => Anchor {
            date: date(&d)?,
            timer: Timer {
                since: now,
                // 停機期間的經過時間上限 24 小時
                carried: Duration::from_secs(elapsed).saturating_add(downtime.min(MAX_CARRIED)),
                since_wall: now_wall,
            },
        },
        // 舊版狀態檔沒有基準點：以已接受的日期與現在為基準（較保守）
        (None, None, None) => {
            rewritten = true;
            Anchor {
                date: accepted,
                timer: Timer::start(now, now_wall),
            }
        }
        _ => return None,
    };
    let jump = match state.jump_since_unix {
        None => None,
        Some(unix) => {
            // 起點只供診斷；在未來時改寫為現在
            let since_wall = wall(unix)?;
            let since_wall = if since_wall > now_wall {
                rewritten = true;
                now_wall
            } else {
                since_wall
            };
            // 已累積的時間 + 停機時間（上限 1 小時）；舊版沒有累積時間時從 0 起算（較保守）
            let carried = Duration::from_secs(state.jump_elapsed_secs.unwrap_or(0))
                .saturating_add(downtime.min(MAX_JUMP_DOWNTIME_CARRY));
            let timer = Timer {
                since: now,
                carried,
                since_wall,
            };
            let jump_date = match state.jump_date {
                Some(d) => Some(date(&d)?),
                None => None,
            };
            Some(Jump {
                timer,
                date: jump_date,
            })
        }
    };
    let rollback = match state.rollback_since_unix {
        None => None,
        Some(unix) => {
            let (timer, changed) = Timer::resume(wall(unix)?, now, now_wall, MAX_CARRIED);
            rewritten |= changed;
            Some(timer)
        }
    };
    Some(Parsed {
        accepted,
        anchor,
        jump,
        rollback,
        rewritten,
    })
}

/// 暫存檔名：`<檔名>.<隨機 16 位十六進位>.tmp`，放在同一個目錄（rename 才是原子的）
pub fn unique_tmp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_u32(std::process::id());
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{}.{:016x}.tmp", name, h.finish()))
}

/// 是否為 [`unique_tmp_path`] 為 `file_name` 產生的暫存檔名（或舊版固定的 `<檔名>.tmp`）
pub fn is_tmp_name_for(candidate: &str, file_name: &str) -> bool {
    let Some(rest) = candidate
        .strip_prefix(file_name)
        .and_then(|r| r.strip_prefix('.'))
    else {
        return false;
    };
    if rest == "tmp" {
        return true;
    }
    rest.strip_suffix(".tmp")
        .is_some_and(|hex| hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// 原子寫入：以 `create_new` 建立隨機檔名的暫存檔（不會跟隨預先放置的符號連結或覆寫既有檔案；
/// R2-K9）、寫入並同步到磁碟後 rename 成 `path`。失敗時刪除暫存檔。同步 I/O，請在 blocking 執行緒呼叫。
pub fn write_atomically(path: &Path, parts: &[&[u8]]) -> std::io::Result<()> {
    let tmp = unique_tmp_path(path);
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        for part in parts {
            file.write_all(part)?;
        }
        file.sync_all()
    })()
    .map_err(|e| Error::new(e.kind(), format!("無法寫入 {}: {}", tmp.display(), e)));
    let result = written.and_then(|()| {
        std::fs::rename(&tmp, path).map_err(|e| {
            Error::new(
                e.kind(),
                format!("無法將 {} 更名為 {}: {}", tmp.display(), path.display(), e),
            )
        })
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 一類依日期命名的檔案
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatedFiles {
    pub prefix: &'static str,
    pub suffix: &'static str,
    /// 日誌中稱呼這類檔案的名稱
    pub label: &'static str,
}

impl DatedFiles {
    /// 某日期的檔名
    pub fn file_name(&self, date: NaiveDate) -> String {
        format!("{}{}{}", self.prefix, date.format("%Y-%m-%d"), self.suffix)
    }

    /// 只有**完全符合** `<prefix>YYYY-MM-DD<suffix>`（且為合法日期）才回傳日期
    pub fn parse(&self, name: &str) -> Option<NaiveDate> {
        let date = name.strip_prefix(self.prefix)?.strip_suffix(self.suffix)?;
        let bytes = date.as_bytes();
        let well_formed = bytes.len() == 10
            && bytes.iter().enumerate().all(|(i, &b)| {
                if i == 4 || i == 7 {
                    b == b'-'
                } else {
                    b.is_ascii_digit()
                }
            });
        if !well_formed {
            return None;
        }
        NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
    }

    /// `dir` 中檔名合法、日期不晚於 `not_after` 的檔案裡最新的日期（沒有時為 `None`）。
    /// 晚於 `not_after` 的檔案（時鐘誤校到未來時建立的）不列入。
    /// 同步讀取目錄，只在啟動時呼叫一次（作為 [`DateGuard`] 沒有狀態檔時的基準）。
    pub fn newest(&self, dir: &Path, not_after: NaiveDate) -> Option<NaiveDate> {
        std::fs::read_dir(dir)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().and_then(|n| self.parse(n)))
            .filter(|date| *date <= not_after)
            .max()
    }

    /// 啟動時 [`DateGuard`] 的基準：既有最新檔案的日期，但不晚於今天
    /// （檔案日期在未來時以今天為準，避免之後的時鐘跳躍被當成正常）；沒有檔案時為今天
    pub fn guard_baseline(&self, newest: Option<NaiveDate>, today: NaiveDate) -> NaiveDate {
        newest.map_or(today, |newest| newest.min(today))
    }

    /// 刪除 `dir` 中過期的檔案，回傳刪除數。檔名不完全符合的檔案一律不碰；刪除失敗只記 warn。
    pub async fn cleanup(&self, dir: &Path, today: NaiveDate, retention_days: u32) -> usize {
        if retention_days == 0 {
            return 0;
        }
        let mut entries = match fs::read_dir(dir).await {
            Ok(entries) => entries,
            Err(e) => {
                warn!(
                    "無法列出 {} 以清除過期的{}: {}",
                    dir.display(),
                    self.label,
                    e
                );
                return 0;
            }
        };
        let mut removed = 0;
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(e) => {
                    warn!("列出 {} 時發生錯誤: {}", dir.display(), e);
                    break;
                }
            };
            let name = entry.file_name();
            let Some(date) = name.to_str().and_then(|n| self.parse(n)) else {
                continue;
            };
            if !should_delete(date, today, retention_days)
                || !entry.file_type().await.is_ok_and(|t| t.is_file())
            {
                continue;
            }
            let path = entry.path();
            match fs::remove_file(&path).await {
                Ok(()) => {
                    removed += 1;
                    info!(
                        "已刪除過期的{} {}（保留 {} 天）",
                        self.label,
                        path.display(),
                        retention_days
                    );
                }
                Err(e) => warn!("無法刪除過期的{} {}: {}", self.label, path.display(), e),
            }
        }
        removed
    }
}

/// 保留判斷：今天與前 `retention_days` 天保留，更早的刪除；`retention_days == 0` 表示永久保留
pub fn should_delete(file_date: NaiveDate, today: NaiveDate, retention_days: u32) -> bool {
    if retention_days == 0 {
        return false;
    }
    today
        .checked_sub_days(Days::new(u64::from(retention_days)))
        .is_some_and(|oldest_kept| file_date < oldest_kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const REPORTS: DatedFiles = DatedFiles {
        prefix: "reports-",
        suffix: ".jsonl",
        label: "report 檔",
    };
    const LOGS: DatedFiles = DatedFiles {
        prefix: "kairos-",
        suffix: ".log",
        label: "日誌檔",
    };

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kairos-retention-{}-{}-{}",
            tag,
            std::process::id(),
            crate::registry::next_conn_id()
        ))
    }

    #[test]
    fn file_names() {
        assert_eq!(
            REPORTS.file_name(date(2026, 10, 9)),
            "reports-2026-10-09.jsonl"
        );
        assert_eq!(
            REPORTS.file_name(date(999, 1, 2)),
            "reports-0999-01-02.jsonl"
        );
        assert_eq!(LOGS.file_name(date(2026, 1, 2)), "kairos-2026-01-02.log");
    }

    #[test]
    fn parsing_is_strict() {
        assert_eq!(
            REPORTS.parse("reports-2026-10-09.jsonl"),
            Some(date(2026, 10, 9))
        );
        assert_eq!(
            REPORTS.parse("reports-2024-02-29.jsonl"),
            Some(date(2024, 2, 29))
        );
        assert_eq!(LOGS.parse("kairos-2026-10-09.log"), Some(date(2026, 10, 9)));
        let d = date(2026, 12, 31);
        assert_eq!(REPORTS.parse(&REPORTS.file_name(d)), Some(d));
        assert_eq!(LOGS.parse(&LOGS.file_name(d)), Some(d));

        for name in [
            "reports.jsonl",
            "reports-2026-10-09.jsonl.bak",
            "reports-2026-10-09.json",
            "reports-2026-10-09.JSONL",
            "Reports-2026-10-09.jsonl",
            "xreports-2026-10-09.jsonl",
            "reports-2026-1-09.jsonl",
            "reports-2026-10-9.jsonl",
            "reports-20261009.jsonl",
            "reports-2026_10_09.jsonl",
            "reports-2026-13-01.jsonl",
            "reports-2026-02-30.jsonl",
            "reports-2025-02-29.jsonl",
            "reports- 2026-10-09.jsonl",
            "reports-+026-10-09.jsonl",
            "reports-2026-10-09 .jsonl",
            "reports-２026-10-09.jsonl",
            "reports-.jsonl",
            "kairos-2026-10-09.log", // 另一類檔案
            "agents.json",
            "",
        ] {
            assert_eq!(REPORTS.parse(name), None, "不應被解析: {name:?}");
        }
        for name in [
            "kairos.log",
            "kairos-2026-10-09.log.1",
            "kairos-2026-10-09.txt",
            "kairos-2026-10-9.log",
            "Kairos-2026-10-09.log",
            "reports-2026-10-09.jsonl",
        ] {
            assert_eq!(LOGS.parse(name), None, "不應被解析: {name:?}");
        }
    }

    const H: Duration = Duration::from_secs(3600);

    fn wall(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    fn plus_days(d: NaiveDate, n: u64) -> NaiveDate {
        d.checked_add_days(Days::new(n)).unwrap()
    }

    /// 不保存狀態、以 2026-10-09 12:00 為基準點的 DateGuard
    fn guard(t0: Instant) -> DateGuard {
        DateGuard::new("report 檔", date(2026, 10, 9), t0, wall(2026, 10, 9, 12))
    }

    #[tokio::test]
    async fn date_guard_allows_normal_progress() {
        // 每小時檢查一次、連續 40 天，日期與實際經過的時間一致：每次都可清除
        let t0 = Instant::now();
        let mut g = guard(t0);
        let base = date(2026, 10, 9);
        for h in 0..(40 * 24u64) {
            let today = plus_days(base, (h + 12) / 24);
            let now = t0 + H * h as u32;
            assert_eq!(
                g.check(today, now, Utc::now()).await,
                Some(today),
                "第 {h} 小時"
            );
        }
        assert_eq!(g.accepted, plus_days(base, (40 * 24 - 1 + 12) / 24));
        // 不超過 ceil(經過時間) + 1 天的跳動也接受（例如換日前後的檢查、時區設定變動）
        let mut g = guard(t0);
        assert_eq!(
            g.check(date(2026, 10, 10), t0, Utc::now()).await,
            Some(date(2026, 10, 10))
        );
        assert_eq!(
            g.check(date(2026, 10, 11), t0 + H, Utc::now()).await,
            Some(date(2026, 10, 11))
        );
        // 日期上限不會溢位
        let mut g = DateGuard::new("x", NaiveDate::MAX, t0, Utc::now());
        assert_eq!(
            g.check(NaiveDate::MAX, t0, Utc::now()).await,
            Some(NaiveDate::MAX)
        );
    }

    #[tokio::test]
    async fn gradual_forward_steps_cannot_outrun_elapsed_time() {
        // R2-K4：每小時把時鐘往前推 2 天（每次都在舊版的 2 天門檻內），15 小時後原本可推進 30 天
        let t0 = Instant::now();
        let mut g = guard(t0);
        let base = date(2026, 10, 9);
        let mut latest_cleanup = base;
        for h in 1..=15u32 {
            let today = plus_days(base, 2 * u64::from(h));
            if let Some(d) = g.check(today, t0 + H * h, Utc::now()).await {
                latest_cleanup = latest_cleanup.max(d);
            }
        }
        // 推進量不超過 ceil(經過時間) + 1 天
        assert!(latest_cleanup <= plus_days(base, 2), "{latest_cleanup}");
        assert!(g.accepted <= plus_days(base, 2));
        // 持續推進時暫停一直重新起算，即使超過 24 小時也不接受
        for h in 16..=60u32 {
            let today = plus_days(base, 2 * u64::from(h));
            assert_eq!(g.check(today, t0 + H * h, Utc::now()).await, None, "{h}");
        }
        // 日期停止變動後持續 24 小時才接受
        // （第 60 小時剛重新起算過；停在比它晚 10 天的日期，第 61 小時再重新起算一次）
        let stopped = plus_days(base, 130);
        assert_eq!(g.check(stopped, t0 + H * 61, Utc::now()).await, None);
        assert_eq!(
            g.check(stopped, t0 + H * 61 + JUMP_ACCEPT_AFTER - H, Utc::now())
                .await,
            None
        );
        assert_eq!(
            g.check(stopped, t0 + H * 61 + JUMP_ACCEPT_AFTER, Utc::now())
                .await,
            Some(stopped)
        );
        // 接受後以新的日期為基準點：之後的正常推進照常接受
        let next = plus_days(stopped, 1);
        assert_eq!(
            g.check(next, t0 + H * 61 + JUMP_ACCEPT_AFTER + H, Utc::now())
                .await,
            Some(next)
        );
    }

    #[tokio::test]
    async fn date_guard_skips_forward_jump_until_it_persists() {
        let t0 = Instant::now();
        let mut g = guard(t0);
        // K-C2：誤校到未來 → 暫停清除
        assert_eq!(g.check(date(2027, 10, 9), t0, Utc::now()).await, None);
        assert_eq!(g.check(date(2027, 10, 9), t0 + H, Utc::now()).await, None);
        // 跳躍日期往回（但仍在合理範圍外）不會重新起算
        assert_eq!(
            g.check(date(2027, 10, 8), t0 + H * 2, Utc::now()).await,
            None
        );
        // 時鐘修正回來 → 恢復，且之後再跳仍會被擋
        assert_eq!(
            g.check(date(2026, 10, 9), t0 + H * 3, Utc::now()).await,
            Some(date(2026, 10, 9))
        );
        let t1 = t0 + H * 4;
        assert_eq!(g.check(date(2026, 10, 13), t1, Utc::now()).await, None);
        // 跳躍時的日期 + 2 天以內的變動繼續計時
        assert_eq!(g.check(date(2026, 10, 15), t1 + H, Utc::now()).await, None);
        // 新的日期持續 24 小時 → 接受
        assert_eq!(
            g.check(date(2026, 10, 15), t1 + JUMP_ACCEPT_AFTER, Utc::now())
                .await,
            Some(date(2026, 10, 15))
        );
        assert_eq!(
            g.check(date(2026, 10, 16), t1 + JUMP_ACCEPT_AFTER, Utc::now())
                .await,
            Some(date(2026, 10, 16))
        );
    }

    #[tokio::test]
    async fn rollback_lowers_accepted_date_after_24_hours() {
        // R2-K6：誤校日期被接受後時鐘改回，24 小時後把 accepted 降回目前日期，保護重新生效
        let t0 = Instant::now();
        let mut g = guard(t0);
        let future = date(2027, 10, 9);
        assert_eq!(g.check(future, t0, Utc::now()).await, None);
        assert_eq!(
            g.check(future, t0 + JUMP_ACCEPT_AFTER, Utc::now()).await,
            Some(future)
        );
        // 時鐘改回：以較早的日期清除（只會刪得更少）
        let t1 = t0 + JUMP_ACCEPT_AFTER + H;
        let real = date(2026, 10, 10);
        assert_eq!(g.check(real, t1, Utc::now()).await, Some(real));
        assert_eq!(g.accepted, future);
        // 24 小時內回到未來日期：視為未變，照常
        assert_eq!(g.check(future, t1 + H, Utc::now()).await, Some(future));
        // 回撥計時重新開始，持續 24 小時後降回
        let t2 = t1 + H * 2;
        assert_eq!(g.check(real, t2, Utc::now()).await, Some(real));
        assert_eq!(
            g.check(real, t2 + ROLLBACK_ACCEPT_AFTER - H, Utc::now())
                .await,
            Some(real)
        );
        assert_eq!(g.accepted, future);
        let t3 = t2 + ROLLBACK_ACCEPT_AFTER;
        let real = date(2026, 10, 11);
        assert_eq!(g.check(real, t3, Utc::now()).await, Some(real));
        assert_eq!(g.accepted, real);
        // 之後再誤校到同一個未來日期 → 再次暫停
        assert_eq!(g.check(future, t3 + H, Utc::now()).await, None);
    }

    fn state_text(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn date_guard_state_survives_restart_during_clock_jump() {
        // K-C2：時鐘誤校到未來期間重新啟動，新實例從狀態檔讀回狀態，不以未來日期為基準清除
        let dir = temp_dir("guard-state");
        let path = dir.join(GUARD_STATE_FILE);
        let t0 = Instant::now();
        let load = |fallback, now, now_wall| {
            DateGuard::load_or_new("report 檔", path.clone(), None, fallback, now, now_wall)
        };

        let mut g = load(date(2026, 10, 8), t0, wall(2026, 10, 9, 12));
        assert_eq!(
            g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 12)).await,
            Some(date(2026, 10, 9))
        );
        // 跳到一年後
        assert_eq!(
            g.check(date(2027, 10, 9), t0, wall(2027, 10, 9, 12)).await,
            None
        );
        drop(g);
        assert_eq!(state_text(&path)["jump_date"], "2027-10-09");

        // 重新啟動：即使沒有狀態檔時的基準會是未來日期，仍以狀態檔為準
        let t1 = Instant::now();
        let mut g = load(date(2027, 10, 9), t1, wall(2027, 10, 9, 13));
        assert_eq!(
            g.check(date(2027, 10, 9), t1, wall(2027, 10, 9, 13)).await,
            None
        );
        drop(g);

        // 隔天重新啟動：停機時間最多算 1 小時，暫停仍持續；執行中再累積到 24 小時才接受
        let t2 = Instant::now();
        let mut g = load(date(2020, 1, 1), t2, wall(2027, 10, 10, 12));
        assert_eq!(
            g.check(date(2027, 10, 10), t2, wall(2027, 10, 10, 12))
                .await,
            None
        );
        // 已累積：重新啟動前 1 小時 + 本次停機 1 小時 = 2 小時；再 22 小時達到 24 小時
        assert_eq!(
            g.check(date(2027, 10, 10), t2 + H * 22 - H, wall(2027, 10, 11, 9))
                .await,
            None
        );
        assert_eq!(
            g.check(date(2027, 10, 11), t2 + H * 22, wall(2027, 10, 11, 10))
                .await,
            Some(date(2027, 10, 11))
        );
        drop(g);
        let state = state_text(&path);
        assert_eq!(state["accepted"], "2027-10-11");
        assert_eq!(state["anchor_date"], "2027-10-11");
        assert!(state["jump_since_unix"].is_null(), "{state}");
        assert!(state["jump_elapsed_secs"].is_null(), "{state}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn restart_with_a_further_jump_restarts_the_pause() {
        // R2-K5：暫停中重新啟動、且時鐘又往前跳超過 2 天：重新起算，而不是以牆鐘差立即接受
        let dir = temp_dir("guard-further");
        let path = dir.join(GUARD_STATE_FILE);
        let load = |now, now_wall| {
            DateGuard::load_or_new(
                "report 檔",
                path.clone(),
                None,
                date(2026, 10, 9),
                now,
                now_wall,
            )
        };
        let t0 = Instant::now();
        let mut g = load(t0, wall(2026, 10, 9, 12));
        assert_eq!(
            g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 12)).await,
            Some(date(2026, 10, 9))
        );
        // 跳到 2026-12-01 → 暫停
        assert_eq!(
            g.check(date(2026, 12, 1), t0, wall(2026, 12, 1, 0)).await,
            None
        );
        drop(g);

        // 重新啟動時時鐘又跳到一年後（牆鐘差約一年）：舊版會立即接受並清除
        let t1 = Instant::now();
        let mut g = load(t1, wall(2027, 12, 1, 0));
        assert_eq!(
            g.check(date(2027, 12, 1), t1, wall(2027, 12, 1, 0)).await,
            None
        );
        drop(g);
        assert_eq!(state_text(&path)["jump_date"], "2027-12-01");

        // 在新的日期停留：每次重新啟動停機時間最多算 1 小時，執行中累積到 24 小時後接受
        let t2 = Instant::now();
        let mut g = load(t2, wall(2027, 12, 1, 23));
        assert_eq!(
            g.check(date(2027, 12, 1), t2, wall(2027, 12, 1, 23)).await,
            None
        );
        drop(g);
        assert_eq!(state_text(&path)["jump_elapsed_secs"], 3600);
        let t3 = Instant::now();
        let mut g = load(t3, wall(2027, 12, 2, 0));
        assert_eq!(
            g.check(date(2027, 12, 2), t3, wall(2027, 12, 2, 0)).await,
            None
        );
        assert_eq!(
            g.check(date(2027, 12, 2), t3 + H * 22, wall(2027, 12, 2, 22))
                .await,
            Some(date(2027, 12, 2))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn adjusting_the_clock_while_stopped_does_not_end_the_pause() {
        // R2-K5：暫停中停機，停機期間把時鐘調快 2 天以內（不會重新起算）再啟動。
        // 若以牆鐘差延續（上限 24 小時 = 接受門檻），重新啟動後會立刻接受並清除
        let dir = temp_dir("guard-stopped-adjust");
        let path = dir.join(GUARD_STATE_FILE);
        let load = |now, now_wall| {
            DateGuard::load_or_new(
                "report 檔",
                path.clone(),
                None,
                date(2026, 10, 9),
                now,
                now_wall,
            )
        };
        let t0 = Instant::now();
        let mut g = load(t0, wall(2026, 10, 9, 12));
        assert!(
            g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 12))
                .await
                .is_some()
        );
        // 時鐘跳到 2027-01-01 → 暫停（執行中只累積 10 分鐘）
        assert_eq!(
            g.check(date(2027, 1, 1), t0, wall(2027, 1, 1, 0)).await,
            None
        );
        let ten_min = Duration::from_secs(600);
        assert_eq!(
            g.check(
                date(2027, 1, 1),
                t0 + ten_min,
                wall(2027, 1, 1, 0) + ten_min
            )
            .await,
            None
        );
        drop(g);
        // 停機期間把時鐘調快 2 天再啟動（牆鐘差約 2 天）
        for h in [0u32, 1, 2] {
            let t1 = Instant::now();
            let mut g = load(t1, wall(2027, 1, 3, h));
            assert_eq!(
                g.check(date(2027, 1, 3), t1, wall(2027, 1, 3, h)).await,
                None,
                "第 {h} 次重新啟動不應接受"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn future_start_times_in_state_are_rewritten_on_load() {
        // R2-K7：狀態檔中的起點在未來（牆鐘往回撥）時改寫為現在並存檔，
        // 否則每次重新啟動計時都歸零，每日排程重啟會讓清除無限期暫停
        let dir = temp_dir("guard-future-since");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(GUARD_STATE_FILE);
        let now_wall = wall(2027, 1, 1, 0);
        let future = (now_wall + chrono::Duration::days(10)).timestamp();
        std::fs::write(
            &path,
            format!(
                r#"{{"accepted":"2026-10-09","anchor_date":"2026-10-09","anchor_elapsed_secs":0,
                    "saved_unix":{future},"jump_since_unix":{future},"jump_date":"2027-01-01",
                    "jump_elapsed_secs":0,"rollback_since_unix":null}}"#
            ),
        )
        .unwrap();
        let load = |now, now_wall| {
            DateGuard::load_or_new(
                "report 檔",
                path.clone(),
                None,
                date(2026, 10, 9),
                now,
                now_wall,
            )
        };
        let t0 = Instant::now();
        let mut g = load(t0, now_wall);
        assert_eq!(g.check(date(2027, 1, 1), t0, now_wall).await, None);
        drop(g);
        let state = state_text(&path);
        assert_eq!(state["jump_since_unix"], now_wall.timestamp());
        assert_eq!(state["saved_unix"], now_wall.timestamp());

        // 之後的重新啟動從改寫後的存檔時間延續（停機最多算 1 小時），執行中累積到 24 小時後接受
        let t1 = Instant::now();
        let later = now_wall + chrono::Duration::hours(24);
        let mut g = load(t1, later);
        assert_eq!(g.check(date(2027, 1, 2), t1, later).await, None);
        assert_eq!(
            g.check(date(2027, 1, 2), t1 + H * 23, later).await,
            Some(date(2027, 1, 2))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn anchor_and_rollback_persist_across_restart() {
        let dir = temp_dir("guard-anchor");
        let path = dir.join(GUARD_STATE_FILE);
        let load = |now, now_wall| {
            DateGuard::load_or_new(
                "report 檔",
                path.clone(),
                None,
                date(2026, 10, 9),
                now,
                now_wall,
            )
        };
        let t0 = Instant::now();
        let mut g = load(t0, wall(2026, 10, 9, 12));
        assert!(
            g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 12))
                .await
                .is_some()
        );
        drop(g);
        // 停機 1 天後重新啟動：以牆鐘差計算經過時間，正常接受
        let t1 = Instant::now();
        let mut g = load(t1, wall(2026, 10, 10, 12));
        assert_eq!(
            g.check(date(2026, 10, 10), t1, wall(2026, 10, 10, 12))
                .await,
            Some(date(2026, 10, 10))
        );
        drop(g);
        // 再停機 3 天：延續量上限 24 小時 → 視為跳躍，暫停 24 小時（第一輪起即有的行為）
        let t1 = Instant::now();
        let mut g = load(t1, wall(2026, 10, 13, 12));
        assert_eq!(
            g.check(date(2026, 10, 13), t1, wall(2026, 10, 13, 12))
                .await,
            None
        );
        assert_eq!(
            g.check(
                date(2026, 10, 14),
                t1 + JUMP_ACCEPT_AFTER,
                wall(2026, 10, 14, 12)
            )
            .await,
            Some(date(2026, 10, 14))
        );
        let t1 = t1 + JUMP_ACCEPT_AFTER;
        // 時鐘回撥：回撥計時寫入狀態檔
        assert_eq!(
            g.check(date(2026, 1, 1), t1 + H, wall(2026, 1, 1, 0)).await,
            Some(date(2026, 1, 1))
        );
        drop(g);
        assert!(state_text(&path)["rollback_since_unix"].is_i64());
        // 重新啟動後延續回撥計時：牆鐘差達 24 小時 → 降回
        let t2 = Instant::now();
        let mut g = load(t2, wall(2026, 1, 2, 0));
        assert_eq!(
            g.check(date(2026, 1, 2), t2, wall(2026, 1, 2, 0)).await,
            Some(date(2026, 1, 2))
        );
        assert_eq!(g.accepted, date(2026, 1, 2));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn missing_or_corrupt_state_falls_back_to_baseline() {
        let dir = temp_dir("guard-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(GUARD_STATE_FILE);
        let t0 = Instant::now();
        let load = || {
            DateGuard::load_or_new(
                "x",
                path.clone(),
                None,
                date(2026, 10, 8),
                t0,
                wall(2026, 10, 9, 0),
            )
        };
        // 不存在：以基準為準，第一次檢查時寫出狀態檔
        let mut g = load();
        assert_eq!(
            g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 0)).await,
            Some(date(2026, 10, 9))
        );
        assert!(path.exists());
        for bad in [
            "not json",
            r#"{"accepted":"2026-13-01","jump_since_unix":null}"#,
            r#"{"accepted":"2026-10-08","anchor_date":"x","anchor_elapsed_secs":1,"saved_unix":1}"#,
            r#"{"accepted":"2026-10-08","anchor_date":"2026-10-08"}"#,
            "{}",
        ] {
            std::fs::write(&path, bad).unwrap();
            let mut g = load();
            // 退回基準：跳到未來仍會被擋
            assert_eq!(
                g.check(date(2027, 1, 1), t0, wall(2027, 1, 1, 0)).await,
                None,
                "{bad}"
            );
            assert_eq!(
                g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 0)).await,
                Some(date(2026, 10, 9))
            );
        }
        // 狀態檔已改寫為有效內容
        assert_eq!(state_text(&path)["accepted"], "2026-10-09");

        // 舊版格式（只有 accepted 與 jump_since_unix）仍可讀取，並補上新欄位
        std::fs::write(&path, r#"{"accepted":"2026-10-09","jump_since_unix":null}"#).unwrap();
        let mut g = load();
        assert_eq!(g.accepted, date(2026, 10, 9));
        assert_eq!(
            g.check(date(2026, 10, 9), t0, wall(2026, 10, 9, 0)).await,
            Some(date(2026, 10, 9))
        );
        assert_eq!(state_text(&path)["anchor_date"], "2026-10-09");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn legacy_state_file_is_read_when_the_new_one_is_missing() {
        // R2-K18：日誌改用自己的狀態檔名；升級時沿用舊檔（不修改舊檔）
        let dir = temp_dir("guard-legacy");
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join(GUARD_STATE_FILE);
        let path = dir.join(LOG_GUARD_STATE_FILE);
        let old = r#"{"accepted":"2026-10-09","jump_since_unix":null}"#;
        std::fs::write(&legacy, old).unwrap();
        let t0 = Instant::now();
        let mut g = DateGuard::load_or_new(
            "日誌檔",
            path.clone(),
            Some(&legacy),
            date(2020, 1, 1),
            t0,
            wall(2026, 10, 9, 0),
        );
        // 以舊檔的日期為準（不是 fallback 的 2020-01-01）：跳到一年後被擋
        assert_eq!(
            g.check(date(2027, 10, 9), t0, wall(2027, 10, 9, 0)).await,
            None
        );
        assert_eq!(state_text(&path)["accepted"], "2026-10-09");
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), old);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn atomic_write_uses_unique_temp_files() {
        // R2-K9：暫存檔為隨機檔名並以 create_new 建立；預先放置的固定檔名不會被使用或覆寫
        let dir = temp_dir("atomic");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("agents.json");
        let decoy = dir.join("agents.json.tmp");
        std::fs::write(&decoy, b"decoy").unwrap();
        write_atomically(&target, &[b"{\"a\":1}", b"\n"]).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"{\"a\":1}\n");
        assert_eq!(std::fs::read(&decoy).unwrap(), b"decoy");
        // 寫完後只剩目標檔與誘餌檔
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");

        let a = unique_tmp_path(&target);
        let b = unique_tmp_path(&target);
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_str().unwrap();
        assert!(is_tmp_name_for(name, "agents.json"), "{name}");
        assert!(is_tmp_name_for("agents.json.tmp", "agents.json"));
        assert!(!is_tmp_name_for("agents.json", "agents.json"));
        assert!(!is_tmp_name_for("agents.json.xyz.tmp", "agents.json"));
        assert!(!is_tmp_name_for(
            "other.json.0123456789abcdef.tmp",
            "agents.json"
        ));

        // 目標目錄不存在時回錯誤，不留下暫存檔
        assert!(write_atomically(&dir.join("missing").join("x.json"), &[b"x"]).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn newest_file_date() {
        let dir = temp_dir("newest");
        let today = date(2026, 10, 9);
        assert_eq!(REPORTS.newest(&dir, today), None);
        fs::create_dir_all(&dir).await.unwrap();
        assert_eq!(REPORTS.newest(&dir, today), None);
        for n in [
            "reports-2026-10-01.jsonl",
            "reports-2026-10-08.jsonl",
            "reports-2027-01-01.jsonl", // 晚於今天：不列入
            "reports-2030-13-01.jsonl",
            "kairos-2031-01-01.log",
        ] {
            fs::write(dir.join(n), b"x\n").await.unwrap();
        }
        assert_eq!(REPORTS.newest(&dir, today), Some(date(2026, 10, 8)));
        assert_eq!(
            REPORTS.newest(&dir, date(2027, 6, 1)),
            Some(date(2027, 1, 1))
        );
        // 基準不晚於今天
        assert_eq!(
            REPORTS.guard_baseline(Some(date(2026, 10, 8)), today),
            date(2026, 10, 8)
        );
        assert_eq!(REPORTS.guard_baseline(Some(date(2030, 1, 1)), today), today);
        assert_eq!(REPORTS.guard_baseline(None, today), today);
        fs::remove_dir_all(&dir).await.unwrap();
    }

    #[test]
    fn retention_boundaries() {
        let today = date(2026, 10, 9);
        assert!(!should_delete(today, today, 30));
        assert!(!should_delete(date(2026, 9, 9), today, 30)); // 剛好 30 天前 → 保留
        assert!(should_delete(date(2026, 9, 8), today, 30)); // 31 天前 → 刪除
        assert!(should_delete(date(2000, 1, 1), today, 30));
        assert!(!should_delete(date(2026, 10, 10), today, 30)); // 未來（時鐘回撥）
        assert!(!should_delete(date(2000, 1, 1), today, 0)); // 0 = 永久保留
        assert!(!should_delete(date(2026, 10, 8), today, 1));
        assert!(should_delete(date(2026, 10, 7), today, 1));
        assert!(!should_delete(date(2025, 12, 31), date(2026, 1, 30), 30)); // 跨年
        assert!(should_delete(date(2025, 12, 30), date(2026, 1, 30), 30));
        // 3650 天：期間含 2020、2024 兩個閏日
        assert!(!should_delete(date(2016, 10, 11), today, 3650));
        assert!(should_delete(date(2016, 10, 10), today, 3650));
        // 日誌預設 14 天
        assert!(!should_delete(date(2026, 9, 25), today, 14));
        assert!(should_delete(date(2026, 9, 24), today, 14));
    }

    #[tokio::test]
    async fn cleanup_deletes_only_expired_well_formed_files() {
        let dir = temp_dir("cleanup");
        fs::create_dir_all(&dir).await.unwrap();
        let names = [
            "kairos-2026-10-09.log",
            "kairos-2026-09-25.log", // 剛好 14 天前 → 保留
            "kairos-2026-09-24.log", // 15 天前 → 刪除
            "kairos-2020-01-01.log", // 刪除
            "kairos.log",            // 舊版 → 不碰
            "kairos-2020-01-01.log.bak",
            "reports-2020-01-01.jsonl", // 另一類 → 不碰
            "notes.txt",
        ];
        for n in names {
            fs::write(dir.join(n), b"x\n").await.unwrap();
        }
        fs::create_dir(dir.join("kairos-2019-01-01.log"))
            .await
            .unwrap();

        assert_eq!(LOGS.cleanup(&dir, date(2026, 10, 9), 14).await, 2);
        for n in names {
            let exists = fs::metadata(dir.join(n)).await.is_ok();
            let deleted = n == "kairos-2026-09-24.log" || n == "kairos-2020-01-01.log";
            assert_eq!(exists, !deleted, "{n}");
        }
        assert!(
            fs::metadata(dir.join("kairos-2019-01-01.log"))
                .await
                .is_ok()
        );
        assert_eq!(LOGS.cleanup(&dir, date(2030, 1, 1), 0).await, 0);
        assert_eq!(
            LOGS.cleanup(&dir.join("missing"), date(2030, 1, 1), 1)
                .await,
            0
        );
        fs::remove_dir_all(&dir).await.unwrap();
    }
}
