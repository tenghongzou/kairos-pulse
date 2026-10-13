//! 依日期命名的檔案（`<prefix>YYYY-MM-DD<suffix>`，日期為 UTC）的保留清除規則，
//! 以及清除前的日期合理性檢查（[`DateGuard`]，R2-K4～K7、R2-P5；商業審計 A8）。
//!
//! Kairos（report 與日誌）與 Pulse（日誌）原本各有一份相同規則的實作，現在共用這裡的邏輯。
//! 本 crate 只做判斷與狀態檔內容的轉換，不讀寫檔案、不記日誌：
//! - 狀態檔的讀寫（原子寫入、暫存檔命名、同步或 async）由各產品負責，
//!   讀入的內容交給 [`DateGuard::from_state`]，寫出的內容取自 [`DateGuard::state`]。
//! - 每次檢查發生的狀態變化以 [`GuardEvent`] 回傳，由各產品以自己的措辭與等級記錄。

#![forbid(unsafe_code)]

mod dated;

pub use dated::{DatedName, guard_baseline, should_delete};

use chrono::{DateTime, Days, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

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

/// 狀態檔內容（JSON）。兩個產品的欄位名稱相同；每次檢查都寫入。
///
/// 舊版 Kairos 的狀態檔只有 `accepted` 與 `jump_since_unix`，其餘欄位缺少時視為未設定
/// （是否接受這種舊格式見 [`LegacyState`]）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardState {
    /// 最近一次接受的日期（YYYY-MM-DD）
    pub accepted: String,
    /// 日期基準點的日期，與存檔當時自基準點以來經過的時間（秒）
    #[serde(default)]
    pub anchor_date: Option<String>,
    #[serde(default)]
    pub anchor_elapsed_secs: Option<u64>,
    /// 存檔時的牆鐘時間（Unix 秒）；重新啟動時以「現在 − 此值」延續基準點的經過時間
    #[serde(default)]
    pub saved_unix: Option<i64>,
    /// 偵測到時鐘跳躍的牆鐘時間（Unix 秒，只供診斷）與當時的日期；沒有跳躍時為 `null`
    #[serde(default)]
    pub jump_since_unix: Option<i64>,
    #[serde(default)]
    pub jump_date: Option<String>,
    /// 存檔時跳躍暫停已累積的時間（秒）；重新啟動時再加上 min(max(0, 現在 − saved_unix), 1 小時)
    #[serde(default)]
    pub jump_elapsed_secs: Option<u64>,
    /// 偵測到日期回撥的牆鐘時間（Unix 秒）；沒有回撥時為 `null`
    #[serde(default)]
    pub rollback_since_unix: Option<i64>,
}

impl GuardState {
    /// 寫入狀態檔的內容：縮排的 JSON 加上結尾換行
    pub fn to_file_bytes(&self) -> serde_json::Result<Vec<u8>> {
        let mut json = serde_json::to_vec_pretty(self)?;
        json.push(b'\n');
        Ok(json)
    }
}

/// 是否接受舊版（Kairos 第二輪審計修正前）的狀態檔格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyState {
    /// 接受（Kairos）：沒有基準點欄位（`anchor_date`、`anchor_elapsed_secs`、`saved_unix` 皆缺少）時
    /// 以已接受的日期與現在為基準點；有 `jump_since_unix` 而沒有 `jump_date` 時，在下一次檢查時補上；
    /// 只有 `jump_date` 時忽略。
    Accept,
    /// 不接受（Pulse，一開始就使用新格式）：基準點三個欄位必須齊全，
    /// `jump_since_unix` 與 `jump_date` 必須同時存在或同時為 `null`，否則視為損毀。
    Reject,
}

/// 計時：本次執行以單調時鐘計算，跨重新啟動以牆鐘差延續
#[derive(Debug, Clone, Copy)]
struct Timer {
    /// 本次執行中開始（或延續）計時的時間（單調時鐘）
    since: Instant,
    /// 先前的執行已經過的時間
    carried: Duration,
    /// 開始計時的牆鐘時間（寫入狀態檔用；基準點不使用）
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
    /// 重新計時（R2-K7；每次檢查都會存檔，否則每次重新啟動都歸零，排程重啟會讓計時永遠到不了）。
    fn resume(
        since_wall: DateTime<Utc>,
        now: Instant,
        now_wall: DateTime<Utc>,
        cap: Duration,
    ) -> Self {
        match (now_wall - since_wall).to_std() {
            Ok(diff) => Self {
                since: now,
                carried: diff.min(cap),
                since_wall,
            },
            Err(_) => Self::start(now, now_wall),
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
    /// 依經過時間推算、目前最晚可接受的日期：`基準日期 + ceil(經過時間 / 1 天) + 1 天`。
    ///
    /// 以奈秒無條件進位：首次啟動（以 2 天前的最新檔案為基準）時經過時間只有幾毫秒，
    /// 以秒計會進位成 0 天而誤報時鐘跳躍。
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

/// 一次檢查中發生的狀態變化（依發生順序），由呼叫端決定記錄的措辭與等級
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardEvent {
    /// 跳躍暫停結束：日期回到合理範圍（`during_rollback` 為 `true` 表示是因日期回撥而結束）
    JumpCleared {
        today: NaiveDate,
        during_rollback: bool,
    },
    /// 開始回撥計時：`today` 早於已接受的日期 `accepted`
    RollbackStarted {
        today: NaiveDate,
        accepted: NaiveDate,
    },
    /// 回撥持續 [`ROLLBACK_ACCEPT_AFTER`]，已接受的日期由 `previous` 降回 `today` 並設為新的基準點
    RollbackAccepted {
        today: NaiveDate,
        previous: NaiveDate,
    },
    /// 回撥計時取消：日期已回到 `accepted`（含）之後
    RollbackCleared { accepted: NaiveDate },
    /// 偵測到時鐘往前跳：`today` 晚於依經過時間推算的最晚日期 `allowed_max`，暫停清除
    JumpStarted {
        today: NaiveDate,
        allowed_max: NaiveDate,
        accepted: NaiveDate,
    },
    /// 暫停中日期比跳躍時看到的 `previous` 再往前超過 [`JUMP_RESTART_DAYS`] 天，重新起算
    JumpRestarted {
        today: NaiveDate,
        previous: NaiveDate,
    },
    /// 跳躍後的日期持續 [`JUMP_ACCEPT_AFTER`]，接受為正確日期並設為新的基準點
    JumpAccepted { today: NaiveDate },
}

/// 一次檢查的結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// 可以清除時用來清除的日期；暫停清除時為 `None`
    pub cleanup_date: Option<NaiveDate>,
    /// 依發生順序的狀態變化
    pub events: Vec<GuardEvent>,
}

/// 保留清除前的日期合理性檢查。
///
/// 系統時鐘被誤校到未來時，以「今天」計算的保留期限會把所有檔案判定為過期而刪光。規則：
///
/// 狀態（每次檢查都寫入狀態檔，重新啟動時讀回）：
/// - `accepted`：最近一次接受的日期（清除以接受的日期為準）。
/// - 基準點 `anchor = (anchor_date, anchor_time)`：此時的日期可信。只在啟動時沒有狀態、
///   跳躍暫停被接受、回撥被接受時設定；正常推進時**不**更新（若每次接受都重設基準，
///   「無條件進位 + 1 天」的寬容會在每次檢查時重複給一次，時鐘每小時往前調 2 天就能逐步推進）。
/// - 跳躍暫停：起點牆鐘時間 `jump_since`、跳躍時看到的日期 `jump_date`。
/// - 回撥計時：起點牆鐘時間 `rollback_since`。
///
/// 經過時間：同一次執行以單調時鐘計；跨重新啟動時，基準點的經過時間為
/// 「存檔時已經過的時間 + min(max(0, 現在 − 存檔時間), 24 小時)」（基準點可能好幾個月都不動，
/// 若直接用「現在 − 基準點的牆鐘時間」，重啟前已偵測到的跳躍會在重啟後被當成經過時間而接受）。
/// 副作用：停機超過約 2 天後重新啟動，會暫停清除 24 小時。
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
/// 跨重新啟動延續跳躍暫停：已累積的時間 + min(max(0, 現在 − 存檔時間), 1 小時)（R2-K5）。
/// 回撥計時延續量為牆鐘差（現在 − 回撥起點）、上限 24 小時；牆鐘差為負（起點在未來）時把起點
/// 改寫為現在（R2-K7）。存檔時間在未來時，停機期間的經過時間視為 0。
///
/// 因此逐步把時鐘往前推（每次 2 天以內）也無法讓 `accepted` 超過實際經過的天數 + 1 天（R2-K4）；
/// 與重新啟動同時發生的時鐘跳躍，最多只能被基準點當成 24 小時的經過時間、被跳躍暫停當成 1 小時。
#[derive(Debug, Clone)]
pub struct DateGuard {
    /// 最近一次接受的日期
    accepted: NaiveDate,
    anchor: Anchor,
    /// 偵測中的跳躍
    jump: Option<Jump>,
    /// 偵測中的日期回撥
    rollback: Option<Timer>,
}

impl DateGuard {
    /// 以 `baseline` 與現在為基準點
    pub fn new(baseline: NaiveDate, now: Instant, now_wall: DateTime<Utc>) -> Self {
        Self {
            accepted: baseline,
            anchor: Anchor {
                date: baseline,
                timer: Timer::start(now, now_wall),
            },
            jump: None,
            rollback: None,
        }
    }

    /// 從狀態檔內容還原；格式不符（不是 JSON、日期或時間無效、欄位組合不合法）時回傳 `None`，
    /// 由呼叫端改以 [`DateGuard::new`] 建立並記錄警告。
    pub fn from_state(
        bytes: &[u8],
        legacy: LegacyState,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Option<Self> {
        let state: GuardState = serde_json::from_slice(bytes).ok()?;
        let date = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok();
        let wall = |unix: i64| DateTime::<Utc>::from_timestamp(unix, 0);
        let accepted = date(&state.accepted)?;
        // 停機期間的牆鐘差（現在 − 存檔時間）；存檔時間在未來時視為 0、沒有存檔時間（舊版）時為 0
        let downtime = match state.saved_unix {
            None => Duration::ZERO,
            Some(saved) => (now_wall - wall(saved)?).to_std().unwrap_or(Duration::ZERO),
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
            (None, None, None) if legacy == LegacyState::Accept => Anchor {
                date: accepted,
                timer: Timer::start(now, now_wall),
            },
            _ => return None,
        };
        let jump_since = match (state.jump_since_unix, &state.jump_date, legacy) {
            (Some(unix), _, LegacyState::Accept) | (Some(unix), Some(_), LegacyState::Reject) => {
                Some(unix)
            }
            (None, _, LegacyState::Accept) | (None, None, LegacyState::Reject) => None,
            // 只有其中一個欄位：視為損毀
            _ => return None,
        };
        let jump = match jump_since {
            None => None,
            Some(unix) => {
                // 起點只供診斷；在未來時改寫為現在
                let since_wall = wall(unix)?.min(now_wall);
                // 已累積的時間 + 停機時間（上限 1 小時）；舊版沒有累積時間時從 0 起算（較保守）
                let carried = Duration::from_secs(state.jump_elapsed_secs.unwrap_or(0))
                    .saturating_add(downtime.min(MAX_JUMP_DOWNTIME_CARRY));
                let jump_date = match state.jump_date {
                    Some(d) => Some(date(&d)?),
                    None => None,
                };
                Some(Jump {
                    timer: Timer {
                        since: now,
                        carried,
                        since_wall,
                    },
                    date: jump_date,
                })
            }
        };
        let rollback = match state.rollback_since_unix {
            None => None,
            Some(unix) => Some(Timer::resume(wall(unix)?, now, now_wall, MAX_CARRIED)),
        };
        Some(Self {
            accepted,
            anchor,
            jump,
            rollback,
        })
    }

    /// 以時鐘的「今天」判斷能否清除（規則見型別說明）；只更新記憶體中的狀態。
    /// 呼叫端應在每次檢查後把 [`DateGuard::state`] 寫入狀態檔。
    pub fn decide(&mut self, today: NaiveDate, now: Instant, now_wall: DateTime<Utc>) -> Decision {
        let mut events = Vec::new();
        let cleanup_date = self.decide_inner(today, now, now_wall, &mut events);
        Decision {
            cleanup_date,
            events,
        }
    }

    fn decide_inner(
        &mut self,
        today: NaiveDate,
        now: Instant,
        now_wall: DateTime<Utc>,
        events: &mut Vec<GuardEvent>,
    ) -> Option<NaiveDate> {
        // 1. 日期回撥
        if today < self.accepted {
            if self.jump.take().is_some() {
                events.push(GuardEvent::JumpCleared {
                    today,
                    during_rollback: true,
                });
            }
            match self.rollback {
                None => {
                    events.push(GuardEvent::RollbackStarted {
                        today,
                        accepted: self.accepted,
                    });
                    self.rollback = Some(Timer::start(now, now_wall));
                }
                Some(timer) if timer.elapsed(now) >= ROLLBACK_ACCEPT_AFTER => {
                    events.push(GuardEvent::RollbackAccepted {
                        today,
                        previous: self.accepted,
                    });
                    self.accept_as_anchor(today, now, now_wall);
                }
                Some(_) => {}
            }
            return Some(today);
        }
        if self.rollback.take().is_some() {
            events.push(GuardEvent::RollbackCleared {
                accepted: self.accepted,
            });
        }

        // 2. 日期未變；3. 推進量在允許範圍內
        if today == self.accepted || today <= self.anchor.allowed_max(now) {
            if self.jump.take().is_some() {
                events.push(GuardEvent::JumpCleared {
                    today,
                    during_rollback: false,
                });
            }
            self.accepted = today;
            return Some(today);
        }

        // 往前跳得比經過的時間多：暫停清除
        let Some(jump) = &mut self.jump else {
            events.push(GuardEvent::JumpStarted {
                today,
                allowed_max: self.anchor.allowed_max(now),
                accepted: self.accepted,
            });
            self.jump = Some(Jump {
                timer: Timer::start(now, now_wall),
                date: Some(today),
            });
            return None;
        };
        let jump_date = *jump.date.get_or_insert(today);
        let restart_after = jump_date
            .checked_add_days(Days::new(JUMP_RESTART_DAYS))
            .unwrap_or(NaiveDate::MAX);
        if today > restart_after {
            events.push(GuardEvent::JumpRestarted {
                today,
                previous: jump_date,
            });
            *jump = Jump {
                timer: Timer::start(now, now_wall),
                date: Some(today),
            };
            None
        } else if jump.timer.elapsed(now) >= JUMP_ACCEPT_AFTER {
            events.push(GuardEvent::JumpAccepted { today });
            self.accept_as_anchor(today, now, now_wall);
            Some(today)
        } else {
            None
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
    }

    /// 要寫入狀態檔的內容（記錄存檔時間與各計時已經過的時間，供重新啟動後延續）
    pub fn state(&self, now: Instant, now_wall: DateTime<Utc>) -> GuardState {
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

    /// 最近一次接受的日期
    pub fn accepted(&self) -> NaiveDate {
        self.accepted
    }

    /// 基準點的日期
    pub fn anchor_date(&self) -> NaiveDate {
        self.anchor.date
    }

    /// 是否正因時鐘跳躍而暫停清除
    pub fn jump_pending(&self) -> bool {
        self.jump.is_some()
    }

    /// 暫停中、跳躍時看到的日期（沒有暫停，或舊版狀態檔尚未補上時為 `None`）
    pub fn jump_date(&self) -> Option<NaiveDate> {
        self.jump.and_then(|j| j.date)
    }

    /// 是否正在進行日期回撥計時
    pub fn rollback_pending(&self) -> bool {
        self.rollback.is_some()
    }
}

#[cfg(test)]
mod tests;
