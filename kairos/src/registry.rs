//! Agent 註冊表：記錄每個 agent_id 的連線與存活狀態。
//!
//! 使用 `std::sync::Mutex`，所有方法都是同步的，鎖不會跨 `.await`。
//! 時間由呼叫端傳入，方便單元測試。

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio_rustls::rustls::pki_types::CertificateDer;

use crate::storage::fmt_time;

/// 離線超過此時間的 agent 會在 sweep 時從註冊表移除
pub const OFFLINE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// 狀態表筆數上限的預設值（與 `limits.max_agents` 的預設相同）
pub const DEFAULT_MAX_AGENTS: usize = 10000;

/// 全域連線編號
static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

/// 取得新的連線編號（每條 TCP 連線一個，從 1 起遞增）
pub fn next_conn_id() -> u64 {
    NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed)
}

/// hello 帶來的 agent 資訊
#[derive(Debug, Clone)]
pub struct HelloInfo {
    pub agent_id: String,
    pub hostname: String,
    pub agent_version: String,
    pub peer: String,
    pub offline_after: Duration,
    /// 握手時取得的客戶端憑證鏈（[0] 為 end-entity）；未使用 mTLS 時為 None
    pub client_chain: Option<ClientChain>,
}

/// 客戶端憑證鏈（共用，避免重新驗證時複製 DER）
pub type ClientChain = Arc<[CertificateDer<'static>]>;

/// 要求連線結束的原因
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KickReason {
    /// 同 agent_id 有新連線
    Superseded,
    /// 重新載入 CA/CRL 後或定期檢查時，客戶端憑證已被撤銷、過期或不再受信任
    CertificateRejected(String),
}

/// 一條現役連線（用於重新驗證客戶端憑證）
#[derive(Debug, Clone)]
pub struct LiveConnection {
    pub agent_id: String,
    pub conn_id: u64,
    pub client_chain: Option<ClientChain>,
}

/// 最近一次合法的 report
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LastReport {
    pub recv_at: String,
    pub seq: u64,
    pub ts: String,
    pub cpu_usage: f32,
    pub total_memory: u64,
    pub used_memory: u64,
}

#[derive(Debug)]
pub struct AgentState {
    pub conn_id: u64,
    pub peer: String,
    pub hostname: String,
    pub agent_version: String,
    pub online: bool,
    pub offline_after: Duration,
    pub last_seen: Instant,
    pub last_seen_wall: DateTime<Utc>,
    pub connected_at: DateTime<Utc>,
    /// 被標記為離線的時間（線上時為 `None`），用於 24 小時清除
    pub offline_since: Option<Instant>,
    pub last_report: Option<LastReport>,
    /// 握手時的客戶端憑證鏈；只在連線期間保留（重新驗證用），斷線時清除
    pub client_chain: Option<ClientChain>,
    /// 連線仍在且尚未被通知結束時為 Some；用來通知該連線結束
    pub kick: Option<oneshot::Sender<KickReason>>,
    /// 連線工作是否仍在（從 register 到 disconnect）。被 kick 後、連線尚未結束前 `kick` 已是
    /// `None`，但這裡仍為 `true`
    pub connected: bool,
}

impl AgentState {
    fn mark_offline(&mut self, now: Instant) {
        self.online = false;
        self.offline_since.get_or_insert(now);
    }

    /// 連線結束：標記離線，並釋放只在連線期間需要的資源
    fn mark_disconnected(&mut self, now: Instant) {
        self.kick = None;
        self.connected = false;
        self.client_chain = None;
        self.mark_offline(now);
    }
}

/// 註冊結果的種類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterKind {
    /// 第一次出現的 agent（或已被清除後再出現）
    New,
    /// 先前離線，現在重新上線
    Reconnected,
    /// 舊連線仍在（含已被通知結束、但尚未關閉的連線），已改由新連線取代
    Superseded { previous_conn_id: u64 },
}

/// 為了騰出空間而從狀態表淘汰的離線 agent
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evicted {
    pub agent_id: String,
    pub hostname: String,
    pub offline_for: Duration,
}

#[derive(Debug)]
pub struct Registration {
    pub kind: RegisterKind,
    /// 被新連線取代時會收到通知
    pub kick: oneshot::Receiver<KickReason>,
    /// 狀態表已滿時，為了這次註冊而淘汰的離線 agent
    pub evicted: Option<Evicted>,
}

/// 狀態表已達上限，且沒有可淘汰的離線 agent
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryFull {
    pub max_agents: usize,
}

/// 以連線身分更新註冊表的結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Touch {
    /// 本連線是現役連線，已更新
    Updated,
    /// 本連線是現役連線，且 agent 先前被 sweep 判定離線，現已恢復上線
    Revived,
    /// 本連線已不是該 agent 的現役連線（已被取代或已被清除），未做任何更新
    Stale,
}

/// sweep 時新判定為離線的 agent
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WentOffline {
    pub agent_id: String,
    pub hostname: String,
    pub silent_for: Duration,
}

/// sweep 時因離線過久而移除的 agent
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub agent_id: String,
    pub hostname: String,
    pub offline_for: Duration,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepResult {
    pub went_offline: Vec<WentOffline>,
    pub removed: Vec<Removed>,
}

/// agents.json 中的單一 agent
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentSnapshot {
    pub agent_id: String,
    pub hostname: String,
    pub agent_version: String,
    pub peer: String,
    pub online: bool,
    pub conn_id: u64,
    pub connected_at: String,
    pub last_seen: String,
    pub offline_after_ms: u64,
    pub last_report: Option<LastReport>,
}

/// agents.json 的完整內容
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RegistrySnapshot {
    pub generated_at: String,
    pub online: usize,
    pub total: usize,
    pub agents: Vec<AgentSnapshot>,
}

/// 持鎖期間只複製原始欄位，時間格式化與排序在鎖外進行
struct RawAgent {
    agent_id: String,
    hostname: String,
    agent_version: String,
    peer: String,
    online: bool,
    conn_id: u64,
    connected_at: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    offline_after: Duration,
    last_report: Option<LastReport>,
}

#[derive(Debug)]
pub struct Registry {
    agents: Mutex<HashMap<String, AgentState>>,
    /// 狀態表最多保留的 agent 數（`limits.max_agents`）
    max_agents: usize,
}

impl Default for Registry {
    fn default() -> Self {
        Self::with_max_agents(DEFAULT_MAX_AGENTS)
    }
}

impl Registry {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_agents(max_agents: usize) -> Self {
        Self {
            agents: Mutex::new(HashMap::new()),
            max_agents: max_agents.max(1),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, AgentState>> {
        // 持鎖期間不會 panic（只有簡單的欄位更新），即使被污染也沿用內部資料
        self.agents.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 以 hello 登記連線。若同 agent_id 已有連線，通知舊連線被取代。
    ///
    /// 新的 agent_id 遇到狀態表已滿時，先淘汰離線最久（且連線已結束）的 agent；
    /// 沒有可淘汰者時回 [`RegistryFull`]，不做任何變更。
    pub fn register(
        &self,
        conn_id: u64,
        info: HelloInfo,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Result<Registration, RegistryFull> {
        let mut agents = self.lock();
        let mut evicted = None;
        if !agents.contains_key(&info.agent_id) && agents.len() >= self.max_agents {
            evicted = Some(
                Self::evict_oldest_offline(&mut agents, now).ok_or(RegistryFull {
                    max_agents: self.max_agents,
                })?,
            );
        }
        let (tx, rx) = oneshot::channel();
        let (kind, last_report) = match agents.get_mut(&info.agent_id) {
            None => (RegisterKind::New, None),
            Some(prev) => {
                let previous_conn_id = prev.conn_id;
                let kind = match prev.kick.take() {
                    // 舊連線仍在：通知它被取代。接收端已結束時 send 會失敗，可忽略。
                    Some(kick) => {
                        let _ = kick.send(KickReason::Superseded);
                        RegisterKind::Superseded { previous_conn_id }
                    }
                    // 舊連線已被通知結束（例如憑證被拒）但尚未關閉：同樣是取代，只是不必再通知
                    None if prev.connected => RegisterKind::Superseded { previous_conn_id },
                    None => RegisterKind::Reconnected,
                };
                (kind, prev.last_report.take())
            }
        };
        agents.insert(
            info.agent_id,
            AgentState {
                conn_id,
                peer: info.peer,
                hostname: info.hostname,
                agent_version: info.agent_version,
                online: true,
                offline_after: info.offline_after,
                last_seen: now,
                last_seen_wall: now_wall,
                connected_at: now_wall,
                offline_since: None,
                last_report,
                client_chain: info.client_chain,
                kick: Some(tx),
                connected: true,
            },
        );
        Ok(Registration {
            kind,
            kick: rx,
            evicted,
        })
    }

    /// 移除連線已結束、離線最久的 agent；沒有可淘汰者時回傳 `None`
    fn evict_oldest_offline(
        agents: &mut HashMap<String, AgentState>,
        now: Instant,
    ) -> Option<Evicted> {
        let agent_id = agents
            .iter()
            .filter(|(_, s)| !s.connected && !s.online)
            .min_by_key(|(_, s)| s.offline_since.unwrap_or(now))
            .map(|(id, _)| id.clone())?;
        let state = agents.remove(&agent_id)?;
        Some(Evicted {
            agent_id,
            hostname: state.hostname,
            offline_for: state
                .offline_since
                .map(|since| now.saturating_duration_since(since))
                .unwrap_or_default(),
        })
    }

    /// 列出仍有連線的 agent 與其客戶端憑證鏈
    pub fn live_connections(&self) -> Vec<LiveConnection> {
        let agents = self.lock();
        agents
            .iter()
            .filter(|(_, s)| s.kick.is_some())
            .map(|(agent_id, s)| LiveConnection {
                agent_id: agent_id.clone(),
                conn_id: s.conn_id,
                client_chain: s.client_chain.clone(),
            })
            .collect()
    }

    /// 通知指定連線結束；conn_id 不符或連線已結束時回傳 `false`
    pub fn kick(&self, agent_id: &str, conn_id: u64, reason: KickReason) -> bool {
        let mut agents = self.lock();
        match agents.get_mut(agent_id) {
            Some(state) if state.conn_id == conn_id => match state.kick.take() {
                Some(kick) => kick.send(reason).is_ok(),
                None => false,
            },
            _ => false,
        }
    }

    /// 收到合法訊息時更新 last_seen。conn_id 不符（已被取代或已被清除）時不做任何更新。
    pub fn touch(
        &self,
        agent_id: &str,
        conn_id: u64,
        now: Instant,
        now_wall: DateTime<Utc>,
    ) -> Touch {
        let mut agents = self.lock();
        match agents.get_mut(agent_id) {
            Some(state) if state.conn_id == conn_id => {
                state.last_seen = now;
                state.last_seen_wall = now_wall;
                let revived = !state.online;
                state.online = true;
                state.offline_since = None;
                if revived {
                    Touch::Revived
                } else {
                    Touch::Updated
                }
            }
            _ => Touch::Stale,
        }
    }

    /// report 已成功放入寫檔佇列後才呼叫，記錄為最近一次 report，確保 agents.json 與
    /// reports.jsonl 一致。conn_id 不符時不更新並回傳 `false`。
    pub fn set_last_report(&self, agent_id: &str, conn_id: u64, report: LastReport) -> bool {
        let mut agents = self.lock();
        match agents.get_mut(agent_id) {
            Some(state) if state.conn_id == conn_id => {
                state.last_report = Some(report);
                true
            }
            _ => false,
        }
    }

    /// 關機時呼叫：把仍標記為線上的 agent 全部標為離線（例如連線工作逾時被中止，
    /// 來不及自行 disconnect）。回傳被標記的數量。
    pub fn mark_all_offline(&self, now: Instant) -> usize {
        let mut agents = self.lock();
        let mut count = 0;
        for state in agents.values_mut() {
            if state.online {
                count += 1;
            }
            state.mark_disconnected(now);
        }
        count
    }

    /// 連線結束。conn_id 不符（舊連線被取代後才結束）時忽略。
    /// 回傳 `true` 表示這次呼叫把 agent 從線上改為離線。
    pub fn disconnect(&self, agent_id: &str, conn_id: u64, now: Instant) -> bool {
        let mut agents = self.lock();
        match agents.get_mut(agent_id) {
            Some(state) if state.conn_id == conn_id => {
                let was_online = state.online;
                state.mark_disconnected(now);
                was_online
            }
            _ => false,
        }
    }

    /// 將 last_seen 超過 offline_after 的線上 agent 標為離線，
    /// 並移除離線超過 [`OFFLINE_RETENTION`] 且已無連線的 agent。
    pub fn sweep(&self, now: Instant) -> SweepResult {
        let mut result = SweepResult::default();
        let mut agents = self.lock();
        agents.retain(|agent_id, state| {
            if state.online {
                let silent_for = now.saturating_duration_since(state.last_seen);
                if silent_for > state.offline_after {
                    state.mark_offline(now);
                    result.went_offline.push(WentOffline {
                        agent_id: agent_id.clone(),
                        hostname: state.hostname.clone(),
                        silent_for,
                    });
                }
                return true;
            }
            // 仍有連線時不移除，等連線結束後再計算
            if state.connected {
                return true;
            }
            let offline_for = state
                .offline_since
                .map(|since| now.saturating_duration_since(since))
                .unwrap_or_default();
            if offline_for > OFFLINE_RETENTION {
                result.removed.push(Removed {
                    agent_id: agent_id.clone(),
                    hostname: state.hostname.clone(),
                    offline_for,
                });
                return false;
            }
            true
        });
        drop(agents);
        result
            .went_offline
            .sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        result.removed.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        result
    }

    /// （線上數, 總數）
    pub fn counts(&self) -> (usize, usize) {
        let agents = self.lock();
        let online = agents.values().filter(|s| s.online).count();
        (online, agents.len())
    }

    /// 產生 agents.json 用的快照（依 agent_id 排序）。
    /// 持鎖期間只複製欄位；時間格式化、排序與序列化都在鎖外進行。
    pub fn snapshot(&self, now_wall: DateTime<Utc>) -> RegistrySnapshot {
        let raw: Vec<RawAgent> = {
            let agents = self.lock();
            agents
                .iter()
                .map(|(agent_id, s)| RawAgent {
                    agent_id: agent_id.clone(),
                    hostname: s.hostname.clone(),
                    agent_version: s.agent_version.clone(),
                    peer: s.peer.clone(),
                    online: s.online,
                    conn_id: s.conn_id,
                    connected_at: s.connected_at,
                    last_seen: s.last_seen_wall,
                    offline_after: s.offline_after,
                    last_report: s.last_report.clone(),
                })
                .collect()
        };

        let mut list: Vec<AgentSnapshot> = raw
            .into_iter()
            .map(|r| AgentSnapshot {
                agent_id: r.agent_id,
                hostname: r.hostname,
                agent_version: r.agent_version,
                peer: r.peer,
                online: r.online,
                conn_id: r.conn_id,
                connected_at: fmt_time(r.connected_at),
                last_seen: fmt_time(r.last_seen),
                offline_after_ms: u64::try_from(r.offline_after.as_millis()).unwrap_or(u64::MAX),
                last_report: r.last_report,
            })
            .collect();
        list.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        RegistrySnapshot {
            generated_at: fmt_time(now_wall),
            online: list.iter().filter(|a| a.online).count(),
            total: list.len(),
            agents: list,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot::error::TryRecvError;

    fn hello(agent_id: &str, peer: &str) -> HelloInfo {
        HelloInfo {
            agent_id: agent_id.into(),
            hostname: format!("host-{agent_id}"),
            agent_version: "0.1.0".into(),
            peer: peer.into(),
            offline_after: Duration::from_millis(5000),
            client_chain: None,
        }
    }

    fn report(seq: u64) -> LastReport {
        LastReport {
            recv_at: "2026-10-08T00:00:00.000Z".into(),
            seq,
            ts: "2026-10-08T00:00:00.000Z".into(),
            cpu_usage: 1.5,
            total_memory: 100,
            used_memory: 50,
        }
    }

    fn ids_offline(r: &SweepResult) -> Vec<&str> {
        r.went_offline.iter().map(|g| g.agent_id.as_str()).collect()
    }

    fn ids_removed(r: &SweepResult) -> Vec<&str> {
        r.removed.iter().map(|g| g.agent_id.as_str()).collect()
    }

    #[test]
    fn conn_ids_are_unique_and_increasing() {
        let a = next_conn_id();
        let b = next_conn_id();
        assert!(b > a);
    }

    #[test]
    fn first_register_is_new_and_online() {
        let r = Registry::new();
        let t0 = Instant::now();
        let reg = r
            .register(1, hello("a", "1.1.1.1:1"), t0, Utc::now())
            .unwrap();
        assert_eq!(reg.kind, RegisterKind::New);
        assert_eq!(r.counts(), (1, 1));
    }

    #[test]
    fn second_connection_supersedes_first() {
        let r = Registry::new();
        let t0 = Instant::now();
        let mut first = r
            .register(1, hello("a", "1.1.1.1:1"), t0, Utc::now())
            .unwrap();
        assert_eq!(first.kick.try_recv(), Err(TryRecvError::Empty));

        let mut second = r
            .register(2, hello("a", "1.1.1.1:2"), t0, Utc::now())
            .unwrap();
        assert_eq!(
            second.kind,
            RegisterKind::Superseded {
                previous_conn_id: 1
            }
        );
        // 舊連線收到 kick
        assert_eq!(first.kick.try_recv(), Ok(KickReason::Superseded));
        // 新連線尚未被踢
        assert_eq!(second.kick.try_recv(), Err(TryRecvError::Empty));

        // 舊連線結束時 conn_id 不符，不得把 agent 標為離線
        assert!(!r.disconnect("a", 1, t0));
        assert_eq!(r.counts(), (1, 1));
        // 舊連線的 touch 也不影響狀態
        assert_eq!(r.touch("a", 1, t0, Utc::now()), Touch::Stale);
        assert_eq!(r.touch("a", 2, t0, Utc::now()), Touch::Updated);

        // 新連線結束才標為離線
        assert!(r.disconnect("a", 2, t0));
        assert_eq!(r.counts(), (0, 1));
        // 重複呼叫不再回報狀態變化
        assert!(!r.disconnect("a", 2, t0));
    }

    #[test]
    fn reconnect_after_disconnect_is_reconnected_and_keeps_last_report() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _first = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        assert_eq!(r.touch("a", 1, t0, Utc::now()), Touch::Updated);
        assert!(r.set_last_report("a", 1, report(7)));
        assert!(r.disconnect("a", 1, t0));

        let reg = r.register(2, hello("a", "p"), t0, Utc::now()).unwrap();
        assert_eq!(reg.kind, RegisterKind::Reconnected);
        let snap = r.snapshot(Utc::now());
        assert_eq!(snap.agents[0].last_report.as_ref().map(|r| r.seq), Some(7));
        assert_eq!(snap.agents[0].conn_id, 2);
        assert!(snap.agents[0].online);
    }

    #[test]
    fn report_from_superseded_connection_is_stale() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        let _b = r.register(2, hello("a", "p"), t0, Utc::now()).unwrap();
        // 舊連線：touch 回傳 Stale（呼叫端據此不寫檔），也不能更新 last_report
        assert_eq!(r.touch("a", 1, t0, Utc::now()), Touch::Stale);
        assert!(!r.set_last_report("a", 1, report(1)));
        assert_eq!(r.snapshot(Utc::now()).agents[0].last_report, None);
        // 現役連線
        assert_eq!(r.touch("a", 2, t0, Utc::now()), Touch::Updated);
        assert!(r.set_last_report("a", 2, report(2)));
        assert_eq!(
            r.snapshot(Utc::now()).agents[0]
                .last_report
                .as_ref()
                .map(|r| r.seq),
            Some(2)
        );
        // 不存在的 agent 也是 Stale
        assert_eq!(r.touch("zzz", 2, t0, Utc::now()), Touch::Stale);
        assert!(!r.set_last_report("zzz", 2, report(3)));
    }

    #[test]
    fn kick_with_reason_and_live_connections() {
        let r = Registry::new();
        let t0 = Instant::now();
        let chain: ClientChain = Arc::from(vec![CertificateDer::from(vec![1u8, 2, 3])]);
        let mut info = hello("a", "p");
        info.client_chain = Some(chain.clone());
        let mut a = r.register(1, info, t0, Utc::now()).unwrap();
        let mut b = r.register(2, hello("b", "p"), t0, Utc::now()).unwrap();

        let mut live = r.live_connections();
        live.sort_by(|x, y| x.agent_id.cmp(&y.agent_id));
        assert_eq!(live.len(), 2);
        assert_eq!(live[0].conn_id, 1);
        assert_eq!(live[0].client_chain.as_deref(), Some(&chain[..]));
        assert!(live[1].client_chain.is_none());

        // conn_id 不符不踢
        assert!(!r.kick("a", 99, KickReason::CertificateRejected("x".into())));
        assert_eq!(a.kick.try_recv(), Err(TryRecvError::Empty));
        // 踢掉 a
        assert!(r.kick("a", 1, KickReason::CertificateRejected("revoked".into())));
        assert_eq!(
            a.kick.try_recv(),
            Ok(KickReason::CertificateRejected("revoked".into()))
        );
        // 已踢過不再出現在現役清單，也不能重複踢
        assert!(!r.kick("a", 1, KickReason::Superseded));
        assert_eq!(r.live_connections().len(), 1);
        assert_eq!(b.kick.try_recv(), Err(TryRecvError::Empty));
        // 連線結束後不在清單中
        r.disconnect("b", 2, t0);
        assert!(r.live_connections().is_empty());
    }

    #[test]
    fn mark_all_offline_on_shutdown() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        let _b = r.register(2, hello("b", "p"), t0, Utc::now()).unwrap();
        assert!(r.disconnect("b", 2, t0));
        assert_eq!(r.mark_all_offline(t0), 1);
        assert_eq!(r.counts(), (0, 2));
        assert!(r.snapshot(Utc::now()).agents.iter().all(|a| !a.online));
        assert_eq!(r.mark_all_offline(t0), 0);
    }

    #[test]
    fn sweep_marks_silent_agents_offline_once() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        let _b = r.register(2, hello("b", "p"), t0, Utc::now()).unwrap();

        // 剛好等於門檻不算離線
        assert_eq!(
            r.sweep(t0 + Duration::from_millis(5000)),
            SweepResult::default()
        );

        // b 在 t0+3s 有心跳
        r.touch("b", 2, t0 + Duration::from_secs(3), Utc::now());

        let res = r.sweep(t0 + Duration::from_millis(5001));
        assert_eq!(ids_offline(&res), ["a"]);
        assert_eq!(res.went_offline[0].silent_for, Duration::from_millis(5001));
        assert_eq!(r.counts(), (1, 2));

        // 已離線者不重複回報
        assert!(r.sweep(t0 + Duration::from_secs(6)).went_offline.is_empty());

        // b 之後也逾時
        let res = r.sweep(t0 + Duration::from_millis(8001));
        assert_eq!(ids_offline(&res), ["b"]);
        assert_eq!(r.counts(), (0, 2));
    }

    #[test]
    fn touch_revives_swept_agent() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        assert_eq!(
            r.touch("a", 1, t0 + Duration::from_secs(1), Utc::now()),
            Touch::Updated
        );
        assert_eq!(r.sweep(t0 + Duration::from_secs(7)).went_offline.len(), 1);
        assert_eq!(
            r.touch("a", 1, t0 + Duration::from_secs(8), Utc::now()),
            Touch::Revived
        );
        assert_eq!(r.counts(), (1, 1));
    }

    #[test]
    fn sweep_respects_per_agent_offline_after() {
        let r = Registry::new();
        let t0 = Instant::now();
        let mut slow = hello("slow", "p");
        slow.offline_after = Duration::from_secs(15);
        let _s = r.register(1, slow, t0, Utc::now()).unwrap();
        let _f = r.register(2, hello("fast", "p"), t0, Utc::now()).unwrap();
        let res = r.sweep(t0 + Duration::from_secs(10));
        assert_eq!(ids_offline(&res), ["fast"]);
    }

    #[test]
    fn agents_offline_over_24h_are_removed() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        let _b = r.register(2, hello("b", "p"), t0, Utc::now()).unwrap();
        let _c = r.register(3, hello("c", "p"), t0, Utc::now()).unwrap();

        // a 在 t0 斷線
        assert!(r.disconnect("a", 1, t0));
        // b 在 t0+10s 由 sweep 判定離線，連線也隨後結束
        let res = r.sweep(t0 + Duration::from_secs(10));
        assert_eq!(ids_offline(&res), ["b", "c"]);
        r.disconnect("b", 2, t0 + Duration::from_secs(11));
        // c 被 sweep 判定離線，但連線仍在（kick 尚在），不可移除

        // 剛好 24 小時不移除
        assert!(r.sweep(t0 + OFFLINE_RETENTION).removed.is_empty());

        // a 超過 24 小時 → 移除；b 的離線時間從 t0+10s 起算，尚未超過
        let res = r.sweep(t0 + OFFLINE_RETENTION + Duration::from_secs(1));
        assert_eq!(ids_removed(&res), ["a"]);
        assert!(res.removed[0].offline_for > OFFLINE_RETENTION);
        assert_eq!(r.counts(), (0, 2));

        let res = r.sweep(t0 + OFFLINE_RETENTION + Duration::from_secs(11));
        assert_eq!(ids_removed(&res), ["b"]);
        // c 仍有連線，保留
        assert_eq!(r.counts(), (0, 1));
        assert_eq!(r.snapshot(Utc::now()).agents[0].agent_id, "c");

        // 被移除的 agent 再出現時視為新 agent
        let reg = r.register(4, hello("a", "p"), t0, Utc::now()).unwrap();
        assert_eq!(reg.kind, RegisterKind::New);
    }

    #[test]
    fn revived_agent_resets_offline_clock() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        assert!(r.disconnect("a", 1, t0));
        // 重新連線後再斷線，離線時間要從新的斷線時間起算
        let t1 = t0 + Duration::from_secs(20 * 60 * 60);
        let _a2 = r.register(2, hello("a", "p"), t1, Utc::now()).unwrap();
        assert!(r.disconnect("a", 2, t1));
        assert!(
            r.sweep(t0 + OFFLINE_RETENTION + Duration::from_secs(1))
                .removed
                .is_empty()
        );
        assert_eq!(
            ids_removed(&r.sweep(t1 + OFFLINE_RETENTION + Duration::from_secs(1))),
            ["a"]
        );
    }

    #[test]
    fn full_registry_evicts_oldest_offline_agent() {
        let r = Registry::with_max_agents(3);
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        let _b = r.register(2, hello("b", "p"), t0, Utc::now()).unwrap();
        let _c = r.register(3, hello("c", "p"), t0, Utc::now()).unwrap();
        // b 先離線、a 後離線，c 仍在線
        assert!(r.disconnect("b", 2, t0 + Duration::from_secs(1)));
        assert!(r.disconnect("a", 1, t0 + Duration::from_secs(2)));

        // 已存在的 agent 重新連線不需淘汰
        let again = r.register(4, hello("a", "p"), t0, Utc::now()).unwrap();
        assert_eq!(again.kind, RegisterKind::Reconnected);
        assert_eq!(again.evicted, None);
        assert!(r.disconnect("a", 4, t0 + Duration::from_secs(3)));

        // 新的 agent：淘汰離線最久的 b
        let d = r
            .register(5, hello("d", "p"), t0 + Duration::from_secs(10), Utc::now())
            .unwrap();
        assert_eq!(d.kind, RegisterKind::New);
        let evicted = d.evicted.unwrap();
        assert_eq!(evicted.agent_id, "b");
        assert_eq!(evicted.offline_for, Duration::from_secs(9));
        assert_eq!(r.counts(), (2, 3));
        let ids: Vec<String> = r
            .snapshot(Utc::now())
            .agents
            .into_iter()
            .map(|a| a.agent_id)
            .collect();
        assert_eq!(ids, ["a", "c", "d"]);
    }

    #[test]
    fn full_registry_rejects_new_agent_when_all_connected() {
        // K-S1：以隨機 agent_id 連線灌爆狀態表時，筆數不會超過上限
        let r = Registry::with_max_agents(2);
        let t0 = Instant::now();
        let _a = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        let _b = r.register(2, hello("b", "p"), t0, Utc::now()).unwrap();
        assert_eq!(
            r.register(3, hello("c", "p"), t0, Utc::now()).unwrap_err(),
            RegistryFull { max_agents: 2 }
        );
        assert_eq!(r.counts(), (2, 2));

        // 被 sweep 判定離線但連線仍在、或已被 kick 但尚未關閉的連線都不可淘汰
        r.sweep(t0 + Duration::from_secs(10));
        assert!(r.kick("a", 1, KickReason::CertificateRejected("x".into())));
        assert!(r.register(3, hello("c", "p"), t0, Utc::now()).is_err());

        // 既有 agent 的取代不受上限影響
        let b2 = r.register(4, hello("b", "p"), t0, Utc::now()).unwrap();
        assert_eq!(
            b2.kind,
            RegisterKind::Superseded {
                previous_conn_id: 2
            }
        );

        // 連線結束後才可淘汰
        r.disconnect("a", 1, t0 + Duration::from_secs(11));
        let c = r.register(5, hello("c", "p"), t0, Utc::now()).unwrap();
        assert_eq!(c.evicted.map(|e| e.agent_id), Some("a".to_string()));

        // 大量隨機 agent_id 輪流連線、斷線：筆數始終不超過上限
        for i in 0..1000u64 {
            let id = format!("flood-{i}");
            if r.register(100 + i, hello(&id, "p"), t0, Utc::now()).is_ok() {
                r.disconnect(&id, 100 + i, t0);
            }
            assert!(r.counts().1 <= 2);
        }
    }

    #[test]
    fn reconnect_while_rejected_connection_is_closing_is_superseded() {
        // K-C8：舊連線已收到 certificate_rejected 但尚未關閉時，新連線應記為「取代」
        let r = Registry::new();
        let t0 = Instant::now();
        let _old = r.register(1, hello("a", "p"), t0, Utc::now()).unwrap();
        assert!(r.kick("a", 1, KickReason::CertificateRejected("revoked".into())));
        let new = r.register(2, hello("a", "p"), t0, Utc::now()).unwrap();
        assert_eq!(
            new.kind,
            RegisterKind::Superseded {
                previous_conn_id: 1
            }
        );
        // 舊連線之後才結束：conn_id 不符，不影響新連線
        assert!(!r.disconnect("a", 1, t0));
        assert_eq!(r.counts(), (1, 1));
    }

    #[test]
    fn client_chain_is_dropped_on_disconnect() {
        // K-S13：斷線後不再保留客戶端憑證鏈
        let r = Registry::new();
        let t0 = Instant::now();
        let chain: ClientChain = Arc::from(vec![CertificateDer::from(vec![1u8, 2, 3])]);
        let mut info = hello("a", "p");
        info.client_chain = Some(chain.clone());
        let _a = r.register(1, info, t0, Utc::now()).unwrap();
        let mut info_b = hello("b", "p");
        info_b.client_chain = Some(chain.clone());
        let _b = r.register(2, info_b, t0, Utc::now()).unwrap();
        assert_eq!(Arc::strong_count(&chain), 3);

        assert!(r.disconnect("a", 1, t0));
        assert!(r.lock().get("a").unwrap().client_chain.is_none());
        r.mark_all_offline(t0);
        assert!(r.lock().get("b").unwrap().client_chain.is_none());
        assert_eq!(Arc::strong_count(&chain), 1);
    }

    #[test]
    fn snapshot_is_sorted_and_counts() {
        let r = Registry::new();
        let t0 = Instant::now();
        let _b = r.register(1, hello("b", "p"), t0, Utc::now()).unwrap();
        let _a = r.register(2, hello("a", "p"), t0, Utc::now()).unwrap();
        r.disconnect("b", 1, t0);
        let snap = r.snapshot(Utc::now());
        assert_eq!(snap.total, 2);
        assert_eq!(snap.online, 1);
        assert_eq!(snap.agents[0].agent_id, "a");
        assert_eq!(snap.agents[1].agent_id, "b");
        assert!(!snap.agents[1].online);
        assert_eq!(snap.agents[0].offline_after_ms, 5000);
    }
}
