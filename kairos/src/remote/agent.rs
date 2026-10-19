//! 8443 連線上的遠端管理（PROTOCOL.md「遠端管理」）：hello 時重新計算這台主機的期望設定、
//! `config_get` → `config`、`config_status` → `ack`。升級的訊息（`upgrade_get`、`upgrade_chunk_get`、
//! `upgrade_status`）的資料庫部分在 [`super::engine`]，連線端的處理在 `connection.rs`。

use serde_json::{Map, Value};
use std::collections::BTreeMap;

use kairos_proto::mgmt::{
    CONFIG_STATE_DISABLED, CONFIG_STATE_REJECTED, CONFIG_STATE_ROLLED_BACK, ConfigStatus,
    RemoteConfig, settings as ws,
};

use super::desired::{self, ConfigSnapshot};
use super::store::{ConfigReport, ConfigTarget};
use super::{
    CODE_CONFIG_DISABLED, CODE_CONFIG_OVERRIDDEN, CODE_CONFIG_REJECTED, CODE_CONFIG_ROLLED_BACK,
    RemoteService,
};
use crate::enroll::db::DbError;

impl RemoteService {
    /// hello（協商了 `config`）：以 hello 的標籤、版本與 `install.config` 重新計算這台主機的期望設定
    /// （群組來自清冊）。hello 的資訊或雜湊改變時才寫入（背景進行，不延遲 `hello_ack`）。回傳
    /// `desired_rev`（`pending.config_rev`）
    pub async fn on_hello_config(
        self: &std::sync::Arc<Self>,
        agent_id: &str,
        tags: Vec<String>,
        agent_version: &str,
        install_config: Option<String>,
    ) -> Result<i64, DbError> {
        let snap = self.snapshot();
        let now = self.now_ms();
        let agent = agent_id.to_string();
        let version = agent_version.to_string();
        let tags2 = tags.clone();
        let ic = install_config.clone();
        let (target, row) = self
            .db
            .read(move |s| {
                let mut t = s.config_target(&agent)?.unwrap_or_else(|| ConfigTarget {
                    agent_id: agent.clone(),
                    ..Default::default()
                });
                t.tags = tags2;
                t.agent_version = Some(version);
                t.install_config = ic;
                let row = s.host_config(&agent)?;
                Ok((t, row))
            })
            .await?;
        let (merged, _, hash) = desired::desired_of(&snap, &target, now);
        let (cur_rev, cur_hash) = row.as_ref().map_or((0, String::new()), |r| {
            (r.desired_rev, r.desired_hash.clone())
        });
        let tags_json = serde_json::to_string(&tags).unwrap_or_else(|_| "[]".into());
        // hello 的資訊沒有變：只有雜湊不同時才寫入（之後設定改變時以記下的標籤重新計算）
        let hello_same = row.as_ref().is_some_and(|r| {
            r.tags.as_deref() == Some(tags_json.as_str())
                && r.agent_version.as_deref() == Some(agent_version)
                && r.install_config == install_config
        });
        if hello_same && hash == cur_hash && (cur_rev != 0 || merged.is_empty()) {
            self.apply_desired(&[(agent_id.to_string(), cur_rev)]);
            return Ok(cur_rev);
        }
        // 標籤、版本或群組改變（或設定剛改變、快照較舊）：以記憶體中的快照回覆 `pending`（不讓 hello_ack 等待
        // 寫入：大量主機同時連線時寫入會排隊），在背景記下 hello 的資訊、以資料庫中最新的設定重新計算這一台
        let rev = if hash == cur_hash && cur_rev != 0 {
            cur_rev
        } else if merged.is_empty() {
            0
        } else {
            snap.rev.max(1)
        };
        self.apply_desired(&[(agent_id.to_string(), rev)]);
        let svc = self.clone();
        let t = target.clone();
        let version = agent_version.to_string();
        let agent = agent_id.to_string();
        tokio::spawn(async move {
            let r = svc
                .in_tx(move |s| {
                    s.upsert_hello_info(
                        &t.agent_id,
                        &tags_json,
                        &version,
                        install_config.as_deref(),
                    )?;
                    let snap = ConfigSnapshot::load(s)?;
                    let changed = desired::recompute(s, &snap, Some(&t), now)?;
                    let rev = match changed.first() {
                        Some((_, rev)) => *rev,
                        None => s.host_config(&t.agent_id)?.map_or(0, |r| r.desired_rev),
                    };
                    Ok((rev, !changed.is_empty()))
                })
                .await;
            match r {
                Ok((rev, did)) => {
                    if did {
                        log::debug!(
                            "集中設定：{agent} 的期望設定改變（標籤、版本或群組），第 {rev} 版"
                        );
                    }
                    svc.apply_desired(&[(agent, rev)]);
                }
                Err(e) => log::warn!(
                    "集中設定：無法保存 {agent} 的 hello 資訊：{}",
                    DbError::from(e)
                ),
            }
        });
        Ok(rev)
    }

    /// `config_get` → `config`：這台主機目前的期望設定（依 `agent_version` 只送該版本認得的鍵）
    pub async fn config_for(
        &self,
        agent_id: &str,
        agent_version: &str,
        seq: u64,
    ) -> Result<RemoteConfig, DbError> {
        let agent = agent_id.to_string();
        let row = self.db.read(move |s| Ok(s.host_config(&agent)?)).await?;
        let (rev, settings, sources) = match row {
            Some(r) if r.desired_rev > 0 => {
                let all = desired::parse_settings(&r.desired);
                let filtered = ws::filter_for_version(&all, agent_version);
                let sources: BTreeMap<String, String> =
                    serde_json::from_str(&r.sources).unwrap_or_default();
                let keys = desired::keys_of(&filtered);
                let sources: BTreeMap<String, String> = sources
                    .into_iter()
                    .filter(|(k, _)| keys.contains(k))
                    .collect();
                (r.desired_rev as u64, filtered, sources)
            }
            _ => (0, Map::new(), BTreeMap::new()),
        };
        let issued = crate::api::http::ts_ms(self.now_ms());
        Ok(RemoteConfig::new(
            seq,
            rev,
            settings,
            (!sources.is_empty()).then_some(sources),
            issued,
        ))
    }

    /// `config_status`：記錄實際生效的值、本機覆蓋、忽略與錯誤（`rev` 比已記錄的舊、又不是目前的期望版本時
    /// 不倒退版本）
    pub async fn record_config_status(
        &self,
        agent_id: &str,
        hostname: &str,
        st: &ConfigStatus,
    ) -> Result<(), DbError> {
        let report = ConfigReport {
            agent_id: agent_id.to_string(),
            rev: st.rev.min(i64::MAX as u64) as i64,
            hash: st.hash.clone(),
            state: st.state.clone(),
            effective: Value::Object(st.effective.clone()).to_string(),
            overridden: to_json(&st.overridden),
            ignored: to_json(&st.ignored),
            errors: to_json(&st.errors),
            code: st.code.clone(),
            reported_at: self.now_ms(),
        };
        let r = report.clone();
        let (prev_state, prev_overridden) = self
            .in_tx(move |s| Ok(s.record_config_status(&r)?))
            .await
            .map_err(DbError::from)?;
        self.emit(
            "config.host",
            serde_json::json!({
                "agent_id": agent_id,
                "state": st.state,
                "applied_rev": st.rev,
                "code": st.code,
                "overridden": st.overridden.len(),
            }),
        );
        // 日誌：狀態改變時記一次（被拒、已回復、停用），本機覆蓋的鍵數改變時記一次
        let changed = prev_state.as_deref() != Some(st.state.as_str());
        match st.state.as_str() {
            CONFIG_STATE_REJECTED => log::warn!(
                event = "config.rejected", code = CODE_CONFIG_REJECTED;
                "警告 {CODE_CONFIG_REJECTED}：{agent_id}（{}）拒絕第 {} 版的集中設定，沿用上一版：{}",
                hostname.escape_debug(),
                st.rev,
                st.errors.iter().map(|e| format!("{} {}", e.key, e.reason)).collect::<Vec<_>>().join("、")
            ),
            CONFIG_STATE_ROLLED_BACK => log::warn!(
                event = "config.rolled_back", code = CODE_CONFIG_ROLLED_BACK;
                "警告 {CODE_CONFIG_ROLLED_BACK}：{agent_id}（{}）套用第 {} 版後失去連線，已回復上一份可用的設定",
                hostname.escape_debug(),
                st.rev
            ),
            CONFIG_STATE_DISABLED if changed => log::info!(
                event = "config.disabled", code = CODE_CONFIG_DISABLED;
                "資訊 {CODE_CONFIG_DISABLED}：集中設定在 {agent_id}（{}）上停用",
                hostname.escape_debug()
            ),
            _ => {}
        }
        if !st.overridden.is_empty() && st.overridden.len() != prev_overridden {
            log::info!(
                event = "config.overridden", code = CODE_CONFIG_OVERRIDDEN;
                "資訊 {CODE_CONFIG_OVERRIDDEN}：{agent_id}（{}）有 {} 個鍵由本機設定覆蓋：{}",
                hostname.escape_debug(),
                st.overridden.len(),
                st.overridden.iter().map(|o| o.key.as_str()).collect::<Vec<_>>().join("、")
            );
        }
        Ok(())
    }
}

/// 序列化成 JSON 文字（失敗時空陣列）
fn to_json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string())
}
