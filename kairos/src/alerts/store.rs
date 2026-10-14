//! 告警的資料表（schema v5，`store/sqlite.rs` 的 `SCHEMA_V5`）：規則、事件、處理紀錄、靜音、通知管道、
//! 發送紀錄、CLI 的動作佇列，以及保留期限與筆數上限的清除。所有函式都是同步的，由呼叫端在 blocking
//! 執行緒上執行（`EnrollDb`）。

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde_json::{Value, json};
use std::collections::HashMap;

use super::engine::{NewDelivery, Timeline};
use super::model::{
    Attention, Category, Channel, ChannelKind, Event, EventState, NotifyPolicy, NotifyState,
    RootCause, Rule, Scope, Severity, Silence, SilenceKind,
};
use super::rules;
use super::secrets::SecretBox;

/// `meta` 的鍵：告警設定（規則、靜音、管道）的版本；CLI 與 API 修改時加一，執行中的引擎依此重新載入
pub const META_CONFIG_REV: &str = "alerts_config_rev";
/// `meta` 的鍵：引擎的心跳（JSON：`at`、`clean`）；啟動時判斷停機期間與是否正常關機
pub const META_HEARTBEAT: &str = "alerts_heartbeat";

fn json_text(v: &impl serde::Serialize) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into())
}

fn parse_json<T: serde::de::DeserializeOwned + Default>(text: Option<String>) -> T {
    text.and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn get_meta(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| {
        r.get::<_, Option<String>>(0)
    })
    .optional()
    .map(Option::flatten)
}

pub fn put_meta(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// 設定的版本（不存在時 0）
pub fn config_rev(conn: &Connection) -> rusqlite::Result<i64> {
    Ok(get_meta(conn, META_CONFIG_REV)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0))
}

/// 設定已修改：版本加一
pub fn bump_config_rev(conn: &Connection) -> rusqlite::Result<()> {
    let rev = config_rev(conn)? + 1;
    put_meta(conn, META_CONFIG_REV, &rev.to_string())
}

// ---------------------------------------------------------------------------
// 規則
// ---------------------------------------------------------------------------

const RULE_COLUMNS: &str = "rule_id, builtin, kind, category, name, enabled, severity, params, scope, channels, notify, description, disabled_reason, created_at, created_by, updated_at, updated_by";

fn rule_of(r: &Row<'_>) -> rusqlite::Result<Rule> {
    Ok(Rule {
        rule_id: r.get(0)?,
        builtin: r.get::<_, i64>(1)? != 0,
        kind: r.get(2)?,
        category: Category::parse(&r.get::<_, String>(3)?).unwrap_or(Category::Host),
        name: r.get(4)?,
        enabled: r.get::<_, i64>(5)? != 0,
        severity: Severity::parse(&r.get::<_, String>(6)?).unwrap_or(Severity::Warning),
        params: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or(Value::Null),
        scope: parse_json::<Scope>(r.get(8)?),
        channels: parse_json::<Vec<String>>(r.get(9)?),
        notify: parse_json::<NotifyPolicy>(r.get(10)?),
        description: r.get(11)?,
        disabled_reason: r.get(12)?,
        created_at: r.get(13)?,
        created_by: r.get(14)?,
        updated_at: r.get(15)?,
        updated_by: r.get(16)?,
    })
}

/// 補齊系統預設規則（已存在的不覆寫）；回傳新增的筆數
pub fn ensure_builtin_rules(conn: &Connection, now_ms: i64) -> rusqlite::Result<usize> {
    let mut n = 0;
    for mut r in rules::builtins() {
        r.created_at = now_ms;
        r.updated_at = now_ms;
        let p = rule_params(&r);
        let refs: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
        n += conn.execute(
            &format!(
                "INSERT OR IGNORE INTO alert_rules ({RULE_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)"
            ),
            refs.as_slice(),
        )?;
    }
    Ok(n)
}

fn rule_params(r: &Rule) -> [Box<dyn rusqlite::ToSql + '_>; 17] {
    [
        Box::new(r.rule_id.clone()),
        Box::new(i64::from(r.builtin)),
        Box::new(r.kind.clone()),
        Box::new(r.category.as_str()),
        Box::new(r.name.clone()),
        Box::new(i64::from(r.enabled)),
        Box::new(r.severity.as_str()),
        Box::new(json_text(&r.params)),
        Box::new(json_text(&r.scope)),
        Box::new(json_text(&r.channels)),
        Box::new(json_text(&r.notify)),
        Box::new(r.description.clone()),
        Box::new(r.disabled_reason.clone()),
        Box::new(r.created_at),
        Box::new(r.created_by.clone()),
        Box::new(r.updated_at),
        Box::new(r.updated_by.clone()),
    ]
}

pub fn load_rules(conn: &Connection) -> rusqlite::Result<Vec<Rule>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RULE_COLUMNS} FROM alert_rules ORDER BY builtin DESC, rule_id"
    ))?;
    let rows = stmt.query_map([], rule_of)?;
    rows.collect()
}

pub fn get_rule(conn: &Connection, id: &str) -> rusqlite::Result<Option<Rule>> {
    conn.query_row(
        &format!("SELECT {RULE_COLUMNS} FROM alert_rules WHERE rule_id = ?1"),
        [id],
        rule_of,
    )
    .optional()
}

/// 新增或整列取代
pub fn save_rule(conn: &Connection, r: &Rule) -> rusqlite::Result<()> {
    let p = rule_params(r);
    let refs: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
    conn.execute(
        &format!(
            "INSERT INTO alert_rules ({RULE_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
             ON CONFLICT (rule_id) DO UPDATE SET
               kind = excluded.kind, category = excluded.category, name = excluded.name,
               enabled = excluded.enabled, severity = excluded.severity, params = excluded.params,
               scope = excluded.scope, channels = excluded.channels, notify = excluded.notify,
               description = excluded.description, disabled_reason = excluded.disabled_reason,
               updated_at = excluded.updated_at, updated_by = excluded.updated_by"
        ),
        refs.as_slice(),
    )?;
    bump_config_rev(conn)
}

pub fn delete_rule(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "DELETE FROM alert_rules WHERE rule_id = ?1 AND builtin = 0",
        [id],
    )?;
    bump_config_rev(conn)?;
    Ok(n > 0)
}

/// 每條規則最近 `days` 天的觸發次數與「雜訊」（10 分鐘內自行恢復且未被確認的比例；S08）
pub fn rule_stats(
    conn: &Connection,
    since_ms: i64,
) -> rusqlite::Result<HashMap<String, (u64, u64)>> {
    let mut stmt = conn.prepare(
        "SELECT rule_id, count(*),
                sum(CASE WHEN acked = 0 AND recovered_at IS NOT NULL AND recovered_at - started_at <= 600000
                    THEN 1 ELSE 0 END)
         FROM alert_events WHERE started_at >= ?1 AND parent_id IS NULL GROUP BY rule_id",
    )?;
    let rows = stmt.query_map([since_ms], |r| {
        Ok((
            r.get::<_, String>(0)?,
            (r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64),
        ))
    })?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// 事件
// ---------------------------------------------------------------------------

pub const EVENT_COLUMNS: &str = "event_id, rule_id, dedup_key, kind, category, severity, attention, state, acked, title, subject, agent_id, code, parent_id, episode, details, started_at, updated_at, last_signal_at, recovered_at, resolved_at, acked_at, acked_by, assignee, eta_at, root_cause, resolve_note, resolved_by, occurrences, reopen_count, flapping, silenced, in_maintenance, snooze_until, notify_state";

pub fn event_of(r: &Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        event_id: r.get(0)?,
        rule_id: r.get(1)?,
        dedup_key: r.get(2)?,
        kind: r.get(3)?,
        category: Category::parse(&r.get::<_, String>(4)?).unwrap_or(Category::Host),
        severity: Severity::parse(&r.get::<_, String>(5)?).unwrap_or(Severity::Warning),
        attention: Attention::parse(&r.get::<_, String>(6)?).unwrap_or(Attention::Action),
        state: EventState::parse(&r.get::<_, String>(7)?).unwrap_or(EventState::Resolved),
        acked: r.get::<_, i64>(8)? != 0,
        title: r.get(9)?,
        subject: r.get(10)?,
        agent_id: r.get(11)?,
        code: r.get(12)?,
        parent_id: r.get(13)?,
        episode: r.get(14)?,
        details: serde_json::from_str(&r.get::<_, String>(15)?).unwrap_or_else(|_| json!({})),
        started_at: r.get(16)?,
        updated_at: r.get(17)?,
        last_signal_at: r.get(18)?,
        recovered_at: r.get(19)?,
        resolved_at: r.get(20)?,
        acked_at: r.get(21)?,
        acked_by: r.get(22)?,
        assignee: r.get(23)?,
        eta_at: r.get(24)?,
        root_cause: r
            .get::<_, Option<String>>(25)?
            .and_then(|s| RootCause::parse(&s)),
        resolve_note: r.get(26)?,
        resolved_by: r.get(27)?,
        occurrences: r.get::<_, i64>(28)? as u64,
        reopen_count: r.get::<_, i64>(29)? as u64,
        flapping: r.get::<_, i64>(30)? != 0,
        silenced: r.get::<_, i64>(31)? != 0,
        in_maintenance: r.get::<_, i64>(32)? != 0,
        snooze_until: r.get(33)?,
        notify: parse_json::<NotifyState>(r.get(34)?),
    })
}

/// 寫回事件（整列）
pub fn upsert_events(tx: &Transaction<'_>, events: &[Event]) -> rusqlite::Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let mut stmt = tx.prepare(&format!(
        "INSERT INTO alert_events ({EVENT_COLUMNS}, sev_rank)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19,
                 ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36)
         ON CONFLICT (event_id) DO UPDATE SET
           severity = excluded.severity, sev_rank = excluded.sev_rank, state = excluded.state,
           acked = excluded.acked, title = excluded.title, code = excluded.code,
           parent_id = excluded.parent_id, episode = excluded.episode, details = excluded.details,
           updated_at = excluded.updated_at, last_signal_at = excluded.last_signal_at,
           recovered_at = excluded.recovered_at, resolved_at = excluded.resolved_at,
           acked_at = excluded.acked_at, acked_by = excluded.acked_by, assignee = excluded.assignee,
           eta_at = excluded.eta_at, root_cause = excluded.root_cause,
           resolve_note = excluded.resolve_note, resolved_by = excluded.resolved_by,
           occurrences = excluded.occurrences, reopen_count = excluded.reopen_count,
           flapping = excluded.flapping, silenced = excluded.silenced,
           in_maintenance = excluded.in_maintenance, snooze_until = excluded.snooze_until,
           notify_state = excluded.notify_state"
    ))?;
    for e in events {
        stmt.execute(params![
            e.event_id,
            e.rule_id,
            e.dedup_key,
            e.kind,
            e.category.as_str(),
            e.severity.as_str(),
            e.attention.as_str(),
            e.state.as_str(),
            i64::from(e.acked),
            e.title,
            e.subject,
            e.agent_id,
            e.code,
            e.parent_id,
            e.episode,
            json_text(&e.details),
            e.started_at,
            e.updated_at,
            e.last_signal_at,
            e.recovered_at,
            e.resolved_at,
            e.acked_at,
            e.acked_by,
            e.assignee,
            e.eta_at,
            e.root_cause.map(RootCause::as_str),
            e.resolve_note,
            e.resolved_by,
            e.occurrences as i64,
            e.reopen_count as i64,
            i64::from(e.flapping),
            i64::from(e.silenced),
            i64::from(e.in_maintenance),
            e.snooze_until,
            json_text(&e.notify),
            e.severity.rank(),
        ])?;
    }
    Ok(())
}

pub fn insert_timeline(tx: &Transaction<'_>, items: &[Timeline]) -> rusqlite::Result<()> {
    let mut stmt = tx.prepare(
        "INSERT INTO alert_timeline (event_id, at, actor, action, details) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for t in items {
        stmt.execute(params![
            t.event_id,
            t.at,
            t.actor,
            t.action,
            json_text(&t.details)
        ])?;
    }
    Ok(())
}

/// 排入通知；回傳每筆的 id（與輸入同順序）
pub fn insert_deliveries(
    tx: &Transaction<'_>,
    items: &[NewDelivery],
) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = tx.prepare(
        "INSERT INTO alert_deliveries (event_id, channel_id, kind, state, urgent, payload, attempts,
            created_at, next_attempt_at, batch_id)
         VALUES (?1, ?2, ?3, 'pending', ?4, ?5, 0, ?6, ?6, NULL)",
    )?;
    let mut ids = Vec::with_capacity(items.len());
    for d in items {
        let mut payload = d.payload.clone();
        if let (Value::Object(m), Some(g)) = (&mut payload, &d.group) {
            m.insert("group".into(), json!(g));
        }
        stmt.execute(params![
            d.event_id,
            d.channel_id,
            d.kind,
            i64::from(d.urgent),
            json_text(&payload),
            d.created_at
        ])?;
        ids.push(tx.last_insert_rowid());
    }
    Ok(ids)
}

/// 尚未解決的事件
pub fn load_open_events(conn: &Connection) -> rusqlite::Result<Vec<Event>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {EVENT_COLUMNS} FROM alert_events WHERE state <> 'resolved'"
    ))?;
    let rows = stmt.query_map([], event_of)?;
    rows.collect()
}

pub fn get_event(conn: &Connection, id: &str) -> rusqlite::Result<Option<Event>> {
    conn.query_row(
        &format!("SELECT {EVENT_COLUMNS} FROM alert_events WHERE event_id = ?1"),
        [id],
        event_of,
    )
    .optional()
}

/// 手動解決時條件仍成立的對象（最近 30 天）：去重鍵 → 異常的開始時間
pub fn load_suppressed(conn: &Connection, since_ms: i64) -> rusqlite::Result<HashMap<String, i64>> {
    let mut stmt = conn.prepare(
        "SELECT dedup_key, max(episode) FROM alert_events
         WHERE resolved_at >= ?1 AND episode IS NOT NULL
           AND root_cause IN ('hardware', 'network', 'unregistered_maintenance', 'software', 'false_positive', 'other')
         GROUP BY dedup_key",
    )?;
    let rows = stmt.query_map([since_ms], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;
    rows.collect()
}

/// 事件編號已用到的流水號（最近幾天）
pub fn load_seq(conn: &Connection, days: &[String]) -> rusqlite::Result<HashMap<String, u64>> {
    let mut out = HashMap::new();
    for d in days {
        let prefix = format!("E-{d}-");
        let max: Option<String> = conn
            .query_row(
                "SELECT max(event_id) FROM alert_events WHERE event_id >= ?1 AND event_id < ?2",
                params![prefix, format!("E-{d}.")],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        // 流水號超過 4 位時字串比較不正確：另外以數值取最大
        let n: Option<i64> = conn
            .query_row(
                "SELECT max(CAST(substr(event_id, 12) AS INTEGER)) FROM alert_events
                 WHERE event_id >= ?1 AND event_id < ?2",
                params![prefix, format!("E-{d}.")],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let seq = n
            .map(|n| n.max(0) as u64)
            .or_else(|| max.and_then(|m| m.rsplit('-').next()?.parse().ok()));
        if let Some(s) = seq {
            out.insert(d.clone(), s);
        }
    }
    Ok(out)
}

/// 處理紀錄（依時間）
pub fn timeline(conn: &Connection, event_id: &str, limit: usize) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT at, actor, action, details FROM alert_timeline WHERE event_id = ?1 ORDER BY id LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![event_id, limit as i64], |r| {
        Ok(json!({
            "at": crate::api::http::ts_ms(r.get::<_, i64>(0)?),
            "actor": r.get::<_, String>(1)?,
            "action": r.get::<_, String>(2)?,
            "details": r.get::<_, Option<String>>(3)?
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .unwrap_or(Value::Null),
        }))
    })?;
    rows.collect()
}

/// 子事件（最多 `limit` 筆）與總數
pub fn children(
    conn: &Connection,
    parent: &str,
    limit: usize,
) -> rusqlite::Result<(Vec<Event>, u64)> {
    let total: i64 = conn.query_row(
        "SELECT count(*) FROM alert_events WHERE parent_id = ?1",
        [parent],
        |r| r.get(0),
    )?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {EVENT_COLUMNS} FROM alert_events WHERE parent_id = ?1
         ORDER BY CASE state WHEN 'resolved' THEN 2 WHEN 'recovered' THEN 1 ELSE 0 END, started_at LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![parent, limit as i64], event_of)?;
    Ok((rows.collect::<rusqlite::Result<_>>()?, total as u64))
}

/// 列表的篩選（S06）
#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    /// `active`（觸發中＋已確認）、`needs_me`、`recovered`（已恢復待結案）、`all`
    pub view: String,
    pub severity: Vec<String>,
    pub state: Vec<String>,
    pub rule: Vec<String>,
    pub category: Vec<String>,
    pub agent_id: Option<String>,
    pub q: Option<String>,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    /// 包含子事件（預設只列頂層）
    pub include_children: bool,
    pub limit: usize,
    /// 游標：(sev_rank, acked, started_at, event_id)
    pub after: Option<(i64, i64, i64, String)>,
}

/// 列表：預設排序（嚴重度 → 未確認優先 → 開始時間由新到舊）與總數
pub fn list_events(conn: &Connection, f: &EventFilter) -> rusqlite::Result<(Vec<Event>, u64)> {
    let mut wh: Vec<String> = Vec::new();
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut push = |cond: String, mut vals: Vec<Box<dyn rusqlite::ToSql>>, wh: &mut Vec<String>| {
        wh.push(cond);
        args.append(&mut vals);
    };
    match f.view.as_str() {
        "active" => wh.push(
            "state IN ('firing', 'acknowledged') AND attention = 'action'".into(),
        ),
        "needs_me" => wh.push(
            "attention = 'action' AND acked = 0 AND (state = 'firing' OR (state = 'recovered' AND severity = 'emergency'))".into(),
        ),
        "recovered" => wh.push("state = 'recovered' AND attention = 'action'".into()),
        _ => {}
    }
    if !f.include_children {
        wh.push("parent_id IS NULL".into());
    }
    let placeholders = |n: usize| vec!["?"; n].join(", ");
    if !f.severity.is_empty() {
        let vals = f
            .severity
            .iter()
            .map(|s| Box::new(s.clone()) as Box<dyn rusqlite::ToSql>)
            .collect();
        push(
            format!("severity IN ({})", placeholders(f.severity.len())),
            vals,
            &mut wh,
        );
    }
    if !f.state.is_empty() {
        let vals = f
            .state
            .iter()
            .map(|s| Box::new(s.clone()) as Box<dyn rusqlite::ToSql>)
            .collect();
        push(
            format!("state IN ({})", placeholders(f.state.len())),
            vals,
            &mut wh,
        );
    }
    if !f.rule.is_empty() {
        let vals = f
            .rule
            .iter()
            .map(|s| Box::new(s.clone()) as Box<dyn rusqlite::ToSql>)
            .collect();
        push(
            format!("rule_id IN ({})", placeholders(f.rule.len())),
            vals,
            &mut wh,
        );
    }
    if !f.category.is_empty() {
        let vals = f
            .category
            .iter()
            .map(|s| Box::new(s.clone()) as Box<dyn rusqlite::ToSql>)
            .collect();
        push(
            format!("category IN ({})", placeholders(f.category.len())),
            vals,
            &mut wh,
        );
    }
    if let Some(a) = &f.agent_id {
        push("agent_id = ?".into(), vec![Box::new(a.clone())], &mut wh);
    }
    if let Some(q) = &f.q {
        let like = format!(
            "%{}%",
            q.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        push(
            "(event_id = ? OR title LIKE ? ESCAPE '\\' OR subject LIKE ? ESCAPE '\\')".into(),
            vec![Box::new(q.clone()), Box::new(like.clone()), Box::new(like)],
            &mut wh,
        );
    }
    if let Some(t) = f.from_ms {
        push("started_at >= ?".into(), vec![Box::new(t)], &mut wh);
    }
    if let Some(t) = f.to_ms {
        push("started_at < ?".into(), vec![Box::new(t)], &mut wh);
    }
    let base_where = if wh.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", wh.join(" AND "))
    };
    let total: i64 = {
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        conn.query_row(
            &format!("SELECT count(*) FROM alert_events {base_where}"),
            refs.as_slice(),
            |r| r.get(0),
        )?
    };
    let mut page_where = wh.clone();
    if let Some((rank, acked, started, id)) = &f.after {
        page_where.push("(sev_rank, acked, -started_at, event_id) > (?, ?, ?, ?)".into());
        args.push(Box::new(*rank));
        args.push(Box::new(*acked));
        args.push(Box::new(-*started));
        args.push(Box::new(id.clone()));
    }
    let page_where = if page_where.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", page_where.join(" AND "))
    };
    args.push(Box::new(f.limit as i64));
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&format!(
        "SELECT {EVENT_COLUMNS} FROM alert_events {page_where}
         ORDER BY sev_rank, acked, -started_at, event_id LIMIT ?"
    ))?;
    let rows = stmt.query_map(refs.as_slice(), event_of)?;
    Ok((rows.collect::<rusqlite::Result<_>>()?, total as u64))
}

/// 各檢視的數量（S06 頁籤、SideNav 徽章；只含需要處理、頂層、未靜音）
pub fn counts(conn: &Connection) -> rusqlite::Result<Value> {
    let q = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0));
    let mut by_sev = serde_json::Map::new();
    for s in Severity::ALL {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM alert_events WHERE state IN ('firing', 'acknowledged')
               AND attention = 'action' AND parent_id IS NULL AND severity = ?1",
            [s.as_str()],
            |r| r.get(0),
        )?;
        by_sev.insert(s.as_str().into(), json!(n));
    }
    Ok(json!({
        "active": q("SELECT count(*) FROM alert_events WHERE state IN ('firing', 'acknowledged') AND attention = 'action' AND parent_id IS NULL")?,
        "active_by_severity": by_sev,
        "unacknowledged": q("SELECT count(*) FROM alert_events WHERE state = 'firing' AND attention = 'action' AND parent_id IS NULL AND silenced = 0")?,
        "badge": q("SELECT count(*) FROM alert_events WHERE state = 'firing' AND attention = 'action' AND parent_id IS NULL AND silenced = 0")?,
        "needs_me": q("SELECT count(*) FROM alert_events WHERE attention = 'action' AND parent_id IS NULL AND acked = 0 AND (state = 'firing' OR (state = 'recovered' AND severity = 'emergency'))")?,
        "recovered": q("SELECT count(*) FROM alert_events WHERE state = 'recovered' AND attention = 'action' AND parent_id IS NULL")?,
        "silenced": q("SELECT count(*) FROM alert_events WHERE state IN ('firing', 'acknowledged') AND silenced = 1 AND parent_id IS NULL")?,
    }))
}

// ---------------------------------------------------------------------------
// 靜音
// ---------------------------------------------------------------------------

const SILENCE_COLUMNS: &str = "silence_id, kind, name, scope, rules, starts_at, ends_at, reason, notify_on_end, exclude_uptime, from_event, created_at, created_by, updated_at, updated_by, ended_at, ended_by, end_reason, end_notified";

fn silence_of(r: &Row<'_>) -> rusqlite::Result<Silence> {
    Ok(Silence {
        silence_id: r.get(0)?,
        kind: SilenceKind::parse(&r.get::<_, String>(1)?).unwrap_or(SilenceKind::Silence),
        name: r.get(2)?,
        scope: parse_json::<Scope>(r.get(3)?),
        rules: parse_json::<Vec<String>>(r.get(4)?),
        starts_at: r.get(5)?,
        ends_at: r.get(6)?,
        reason: r.get(7)?,
        notify_on_end: r.get::<_, i64>(8)? != 0,
        exclude_uptime: r.get::<_, i64>(9)? != 0,
        from_event: r.get(10)?,
        created_at: r.get(11)?,
        created_by: r.get(12)?,
        updated_at: r.get(13)?,
        updated_by: r.get(14)?,
        ended_at: r.get(15)?,
        ended_by: r.get(16)?,
        end_reason: r.get(17)?,
        end_notified: r.get::<_, i64>(18)? != 0,
    })
}

/// 結束時間在 `since_ms` 之後的靜音（引擎：進行中、即將開始與剛結束的）
pub fn load_silences(conn: &Connection, since_ms: i64) -> rusqlite::Result<Vec<Silence>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SILENCE_COLUMNS} FROM alert_silences
         WHERE min(ends_at, coalesce(ended_at, ends_at)) >= ?1 ORDER BY starts_at"
    ))?;
    let rows = stmt.query_map([since_ms], silence_of)?;
    rows.collect()
}

/// 列表（`status`：`active`、`upcoming`、`ended`、`all`）
pub fn list_silences(
    conn: &Connection,
    status: &str,
    now_ms: i64,
    limit: usize,
) -> rusqlite::Result<Vec<Silence>> {
    let cond = match status {
        "active" => "starts_at <= ?1 AND ?1 < min(ends_at, coalesce(ended_at, ends_at))",
        "upcoming" => "starts_at > ?1 AND ended_at IS NULL",
        "ended" => "min(ends_at, coalesce(ended_at, ends_at)) <= ?1",
        _ => "?1 = ?1",
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT {SILENCE_COLUMNS} FROM alert_silences WHERE {cond} ORDER BY starts_at DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![now_ms, limit as i64], silence_of)?;
    rows.collect()
}

pub fn get_silence(conn: &Connection, id: &str) -> rusqlite::Result<Option<Silence>> {
    conn.query_row(
        &format!("SELECT {SILENCE_COLUMNS} FROM alert_silences WHERE silence_id = ?1"),
        [id],
        silence_of,
    )
    .optional()
}

pub fn save_silence(conn: &Connection, s: &Silence) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO alert_silences ({SILENCE_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
             ON CONFLICT (silence_id) DO UPDATE SET
               name = excluded.name, scope = excluded.scope, rules = excluded.rules,
               starts_at = excluded.starts_at, ends_at = excluded.ends_at, reason = excluded.reason,
               notify_on_end = excluded.notify_on_end, exclude_uptime = excluded.exclude_uptime,
               updated_at = excluded.updated_at, updated_by = excluded.updated_by,
               ended_at = excluded.ended_at, ended_by = excluded.ended_by,
               end_reason = excluded.end_reason, end_notified = excluded.end_notified"
        ),
        params![
            s.silence_id,
            s.kind.as_str(),
            s.name,
            json_text(&s.scope),
            json_text(&s.rules),
            s.starts_at,
            s.ends_at,
            s.reason,
            i64::from(s.notify_on_end),
            i64::from(s.exclude_uptime),
            s.from_event,
            s.created_at,
            s.created_by,
            s.updated_at,
            s.updated_by,
            s.ended_at,
            s.ended_by,
            s.end_reason,
            i64::from(s.end_notified),
        ],
    )?;
    bump_config_rev(conn)
}

pub fn mark_silences_notified(conn: &Connection, ids: &[String]) -> rusqlite::Result<()> {
    for id in ids {
        conn.execute(
            "UPDATE alert_silences SET end_notified = 1 WHERE silence_id = ?1",
            [id],
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 通知管道
// ---------------------------------------------------------------------------

const CHANNEL_COLUMNS: &str = "channel_id, name, kind, enabled, config, secrets, state, consecutive_failures, failed_since, last_attempt_at, last_success_at, last_error_code, last_error, created_at, created_by, updated_at, updated_by";

fn channel_of(r: &Row<'_>, secrets: Option<&SecretBox>) -> rusqlite::Result<Channel> {
    let blob: Option<Vec<u8>> = r.get(5)?;
    let secrets = match (blob, secrets) {
        (Some(b), Some(sb)) => sb.unseal(&b).unwrap_or_else(|e| {
            log::warn!(
                event = "alerts.channel.decrypt_failed";
                "通知管道 {} 的機密無法解密：{e}",
                r.get::<_, String>(0).unwrap_or_default()
            );
            json!({})
        }),
        _ => json!({}),
    };
    Ok(Channel {
        channel_id: r.get(0)?,
        name: r.get(1)?,
        kind: ChannelKind::parse(&r.get::<_, String>(2)?).unwrap_or(ChannelKind::Webhook),
        enabled: r.get::<_, i64>(3)? != 0,
        config: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_else(|_| json!({})),
        secrets,
        state: r.get(6)?,
        consecutive_failures: r.get::<_, i64>(7)? as u32,
        failed_since: r.get(8)?,
        last_attempt_at: r.get(9)?,
        last_success_at: r.get(10)?,
        last_error_code: r.get(11)?,
        last_error: r.get(12)?,
        created_at: r.get(13)?,
        created_by: r.get(14)?,
        updated_at: r.get(15)?,
        updated_by: r.get(16)?,
    })
}

/// 所有管道；`secrets` 為 `None` 時不解密（列表與 API 只需要遮蔽值）
pub fn load_channels(
    conn: &Connection,
    secrets: Option<&SecretBox>,
) -> rusqlite::Result<Vec<Channel>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CHANNEL_COLUMNS} FROM alert_channels ORDER BY name, channel_id"
    ))?;
    let rows = stmt.query_map([], |r| channel_of(r, secrets))?;
    rows.collect()
}

pub fn get_channel(
    conn: &Connection,
    id: &str,
    secrets: Option<&SecretBox>,
) -> rusqlite::Result<Option<Channel>> {
    conn.query_row(
        &format!("SELECT {CHANNEL_COLUMNS} FROM alert_channels WHERE channel_id = ?1"),
        [id],
        |r| channel_of(r, secrets),
    )
    .optional()
}

/// 新增或更新管道的設定與機密（`sealed` 為 `None` 時保留原本的機密）
pub fn save_channel(
    conn: &Connection,
    c: &Channel,
    sealed: Option<Vec<u8>>,
) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO alert_channels ({CHANNEL_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
             ON CONFLICT (channel_id) DO UPDATE SET
               name = excluded.name, enabled = excluded.enabled, config = excluded.config,
               secrets = coalesce(excluded.secrets, alert_channels.secrets),
               updated_at = excluded.updated_at, updated_by = excluded.updated_by"
        ),
        params![
            c.channel_id,
            c.name,
            c.kind.as_str(),
            i64::from(c.enabled),
            json_text(&c.config),
            sealed,
            c.state,
            i64::from(c.consecutive_failures),
            c.failed_since,
            c.last_attempt_at,
            c.last_success_at,
            c.last_error_code,
            c.last_error,
            c.created_at,
            c.created_by,
            c.updated_at,
            c.updated_by,
        ],
    )?;
    bump_config_rev(conn)
}

pub fn delete_channel(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    let n = conn.execute("DELETE FROM alert_channels WHERE channel_id = ?1", [id])?;
    bump_config_rev(conn)?;
    Ok(n > 0)
}

/// 派送的結果寫回管道的狀態（不改版本：引擎在下一個 tick 以派送端的狀態為準）
pub fn update_channel_status(conn: &Connection, c: &Channel) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE alert_channels SET state = ?2, consecutive_failures = ?3, failed_since = ?4,
            last_attempt_at = ?5, last_success_at = ?6, last_error_code = ?7, last_error = ?8
         WHERE channel_id = ?1",
        params![
            c.channel_id,
            c.state,
            i64::from(c.consecutive_failures),
            c.failed_since,
            c.last_attempt_at,
            c.last_success_at,
            c.last_error_code,
            c.last_error,
        ],
    )?;
    Ok(())
}

/// 使用這個管道的規則（刪除管道時列出受影響的規則，S11 L2）
pub fn rules_using_channel(conn: &Connection, id: &str) -> rusqlite::Result<Vec<(String, String)>> {
    Ok(load_rules(conn)?
        .into_iter()
        .filter(|r| {
            r.channels.iter().any(|c| c == id) || r.notify.escalate_channels.iter().any(|c| c == id)
        })
        .map(|r| (r.rule_id, r.name))
        .collect())
}

// ---------------------------------------------------------------------------
// 發送紀錄
// ---------------------------------------------------------------------------

/// 一筆發送紀錄
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    pub id: i64,
    pub event_id: Option<String>,
    pub channel_id: String,
    pub kind: String,
    pub state: String,
    pub urgent: bool,
    pub payload: Value,
    pub attempts: u32,
    pub created_at: i64,
    pub next_attempt_at: i64,
    pub sent_at: Option<i64>,
    pub batch_id: Option<i64>,
    pub response_code: Option<i64>,
    pub error_code: Option<String>,
    pub error: Option<String>,
}

const DELIVERY_COLUMNS: &str = "id, event_id, channel_id, kind, state, urgent, payload, attempts, created_at, next_attempt_at, sent_at, batch_id, response_code, error_code, error";

fn delivery_of(r: &Row<'_>) -> rusqlite::Result<Delivery> {
    Ok(Delivery {
        id: r.get(0)?,
        event_id: r.get(1)?,
        channel_id: r.get(2)?,
        kind: r.get(3)?,
        state: r.get(4)?,
        urgent: r.get::<_, i64>(5)? != 0,
        payload: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or(Value::Null),
        attempts: r.get::<_, i64>(7)? as u32,
        created_at: r.get(8)?,
        next_attempt_at: r.get(9)?,
        sent_at: r.get(10)?,
        batch_id: r.get(11)?,
        response_code: r.get(12)?,
        error_code: r.get(13)?,
        error: r.get(14)?,
    })
}

/// 待送的通知（含暫緩的）
pub fn pending_deliveries(conn: &Connection) -> rusqlite::Result<Vec<Delivery>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {DELIVERY_COLUMNS} FROM alert_deliveries WHERE state IN ('pending', 'held') ORDER BY id"
    ))?;
    let rows = stmt.query_map([], delivery_of)?;
    rows.collect()
}

/// 發送的結果
pub fn update_delivery(conn: &Connection, d: &Delivery) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE alert_deliveries SET state = ?2, attempts = ?3, next_attempt_at = ?4, sent_at = ?5,
            batch_id = ?6, response_code = ?7, error_code = ?8, error = ?9
         WHERE id = ?1",
        params![
            d.id,
            d.state,
            i64::from(d.attempts),
            d.next_attempt_at,
            d.sent_at,
            d.batch_id,
            d.response_code,
            d.error_code,
            d.error,
        ],
    )?;
    Ok(())
}

/// 一個事件的發送紀錄（S07「通知發送狀態」）
pub fn deliveries_of_event(
    conn: &Connection,
    event_id: &str,
    limit: usize,
) -> rusqlite::Result<Vec<Delivery>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {DELIVERY_COLUMNS} FROM alert_deliveries WHERE event_id = ?1 ORDER BY id DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![event_id, limit as i64], delivery_of)?;
    rows.collect()
}

/// 一個管道最近的發送紀錄
pub fn deliveries_of_channel(
    conn: &Connection,
    channel_id: &str,
    limit: usize,
) -> rusqlite::Result<Vec<Delivery>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {DELIVERY_COLUMNS} FROM alert_deliveries WHERE channel_id = ?1 ORDER BY id DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![channel_id, limit as i64], delivery_of)?;
    rows.collect()
}

/// 發送紀錄的 JSON（不含內容：內容可以從事件重建，且可能含有主機的 IP）
pub fn delivery_json(d: &Delivery, channel_name: Option<&str>) -> Value {
    let t = crate::api::http::ts_ms;
    json!({
        "id": d.id,
        "event_id": d.event_id,
        "channel_id": d.channel_id,
        "channel_name": channel_name,
        "kind": d.kind,
        "state": d.state,
        "attempts": d.attempts,
        "created_at": t(d.created_at),
        "next_attempt_at": (d.state == "pending" || d.state == "held").then(|| t(d.next_attempt_at)),
        "sent_at": d.sent_at.map(t),
        "response_code": d.response_code,
        "error_code": d.error_code,
        "error": d.error,
    })
}

// ---------------------------------------------------------------------------
// CLI 的動作佇列
// ---------------------------------------------------------------------------

/// 一個待處理的動作（`kairos alerts ack` 等由執行中的 Kairos 處理）
#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub id: i64,
    pub created_at: i64,
    pub actor: String,
    pub event_id: String,
    pub action: String,
    pub params: Value,
}

pub fn enqueue_action(
    conn: &Connection,
    now_ms: i64,
    actor: &str,
    event_id: &str,
    action: &str,
    p: &Value,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO alert_actions (created_at, actor, event_id, action, params) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![now_ms, actor, event_id, action, json_text(p)],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn pending_actions(conn: &Connection) -> rusqlite::Result<Vec<Action>> {
    let mut stmt = conn.prepare(
        "SELECT id, created_at, actor, event_id, action, params FROM alert_actions
         WHERE processed_at IS NULL ORDER BY id LIMIT 1000",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Action {
            id: r.get(0)?,
            created_at: r.get(1)?,
            actor: r.get(2)?,
            event_id: r.get(3)?,
            action: r.get(4)?,
            params: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(Value::Null),
        })
    })?;
    rows.collect()
}

pub fn finish_action(
    conn: &Connection,
    id: i64,
    now_ms: i64,
    result: &Value,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE alert_actions SET processed_at = ?2, result = ?3 WHERE id = ?1",
        params![id, now_ms, json_text(result)],
    )?;
    Ok(())
}

/// 動作的結果（CLI 等待用）：`None` 為尚未處理
pub fn action_result(conn: &Connection, id: i64) -> rusqlite::Result<Option<Value>> {
    Ok(conn
        .query_row(
            "SELECT result FROM alert_actions WHERE id = ?1 AND processed_at IS NOT NULL",
            [id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
        .and_then(|t| serde_json::from_str(&t).ok()))
}

// ---------------------------------------------------------------------------
// 保留期限與筆數上限
// ---------------------------------------------------------------------------

/// 清除的結果
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Purged {
    pub events: u64,
    pub timeline: u64,
    pub deliveries: u64,
    pub silences: u64,
    pub actions: u64,
}

/// 刪除一批事件與其處理紀錄、發送紀錄
fn purge_events(
    tx: &Transaction<'_>,
    cond: &str,
    arg: i64,
    batch: i64,
    out: &mut Purged,
) -> rusqlite::Result<u64> {
    tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS purge_ids (id TEXT PRIMARY KEY) WITHOUT ROWID; DELETE FROM purge_ids;")?;
    tx.execute(
        &format!("INSERT INTO purge_ids SELECT event_id FROM alert_events WHERE {cond} LIMIT ?2"),
        params![arg, batch],
    )?;
    out.timeline += tx.execute(
        "DELETE FROM alert_timeline WHERE event_id IN (SELECT id FROM purge_ids)",
        [],
    )? as u64;
    out.deliveries += tx.execute(
        "DELETE FROM alert_deliveries WHERE event_id IN (SELECT id FROM purge_ids)",
        [],
    )? as u64;
    let n = tx.execute(
        "DELETE FROM alert_events WHERE event_id IN (SELECT id FROM purge_ids)",
        [],
    )? as u64;
    out.events += n;
    Ok(n)
}

/// 依保留天數與筆數上限刪除已解決的事件（最多 `batch` 筆；回傳是否還有要刪的）
pub fn purge(
    conn: &mut Connection,
    now_ms: i64,
    retention_days: u32,
    max_events: u64,
    batch: i64,
) -> rusqlite::Result<(Purged, bool)> {
    let mut out = Purged::default();
    let before = now_ms - i64::from(retention_days) * 86_400_000;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut more = purge_events(
        &tx,
        "state = 'resolved' AND resolved_at < ?1",
        before,
        batch,
        &mut out,
    )? as i64
        >= batch;
    // 筆數上限：刪除最舊的已解決事件
    let total: i64 = tx.query_row("SELECT count(*) FROM alert_events", [], |r| r.get(0))?;
    let over = total - max_events as i64;
    if over > 0 {
        let n = over.min(batch);
        let cutoff: Option<i64> = tx
            .query_row(
                "SELECT resolved_at FROM alert_events WHERE state = 'resolved'
                 ORDER BY resolved_at LIMIT 1 OFFSET ?1",
                [n - 1],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if let Some(c) = cutoff {
            purge_events(
                &tx,
                "state = 'resolved' AND resolved_at <= ?1",
                c,
                n,
                &mut out,
            )?;
            more |= over > batch;
        }
    }
    // 測試通知、靜音結束等沒有事件的發送紀錄：保留 30 天（不超過保留天數）
    let keep = now_ms - i64::from(retention_days.min(30)) * 86_400_000;
    out.deliveries += tx.execute(
        "DELETE FROM alert_deliveries WHERE event_id IS NULL AND created_at < ?1 AND state NOT IN ('pending', 'held')",
        [keep],
    )? as u64;
    out.silences += tx.execute(
        "DELETE FROM alert_silences WHERE min(ends_at, coalesce(ended_at, ends_at)) < ?1",
        [before],
    )? as u64;
    out.actions += tx.execute(
        "DELETE FROM alert_actions WHERE processed_at IS NOT NULL AND processed_at < ?1",
        [now_ms - 7 * 86_400_000],
    )? as u64;
    tx.commit()?;
    Ok((out, more))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::model::{Attention, Category, EventState, NotifyState, Severity};

    fn db() -> (std::path::PathBuf, Connection) {
        let dir = std::env::temp_dir().join(format!(
            "kairos-alert-store-{}-{}",
            std::process::id(),
            crate::registry::next_conn_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut conn = Connection::open(dir.join("k.db")).unwrap();
        crate::store::sqlite::migrate(&mut conn, crate::store::sqlite::MIGRATIONS).unwrap();
        (dir, conn)
    }

    fn event(
        id: &str,
        sev: Severity,
        state: EventState,
        started: i64,
        resolved: Option<i64>,
    ) -> Event {
        Event {
            event_id: id.into(),
            rule_id: "sys.host_offline".into(),
            dedup_key: format!("k|{id}"),
            kind: "host_offline".into(),
            category: Category::Host,
            severity: sev,
            attention: Attention::Action,
            state,
            acked: false,
            title: format!("主機離線：{id}"),
            subject: id.into(),
            agent_id: Some(id.into()),
            code: None,
            parent_id: None,
            episode: Some(started),
            details: json!({}),
            started_at: started,
            updated_at: started,
            last_signal_at: started,
            recovered_at: resolved,
            resolved_at: resolved,
            acked_at: None,
            acked_by: None,
            assignee: None,
            eta_at: None,
            root_cause: resolved.map(|_| RootCause::SelfRecovered),
            resolve_note: None,
            resolved_by: None,
            occurrences: 1,
            reopen_count: 0,
            flapping: false,
            silenced: false,
            in_maintenance: false,
            snooze_until: None,
            notify: NotifyState::default(),
        }
    }

    #[test]
    fn listing_sorts_by_severity_and_pages_with_a_cursor() {
        let (dir, mut conn) = db();
        let tx = conn.transaction().unwrap();
        upsert_events(
            &tx,
            &[
                event("E-1", Severity::Warning, EventState::Firing, 100, None),
                event("E-2", Severity::Emergency, EventState::Firing, 50, None),
                event("E-3", Severity::Critical, EventState::Firing, 300, None),
                event("E-4", Severity::Critical, EventState::Firing, 200, None),
                event(
                    "E-5",
                    Severity::Critical,
                    EventState::Resolved,
                    10,
                    Some(20),
                ),
            ],
        )
        .unwrap();
        tx.commit().unwrap();
        let mut f = EventFilter {
            view: "active".into(),
            limit: 2,
            ..EventFilter::default()
        };
        let (page, total) = list_events(&conn, &f).unwrap();
        assert_eq!(total, 4);
        let ids: Vec<&str> = page.iter().map(|e| e.event_id.as_str()).collect();
        // 嚴重度 → 開始時間由新到舊
        assert_eq!(ids, ["E-2", "E-3"]);
        let last = page.last().unwrap();
        f.after = Some((
            last.severity.rank(),
            0,
            last.started_at,
            last.event_id.clone(),
        ));
        let (page, _) = list_events(&conn, &f).unwrap();
        let ids: Vec<&str> = page.iter().map(|e| e.event_id.as_str()).collect();
        assert_eq!(ids, ["E-4", "E-1"]);
        f.view = "all".into();
        f.after = None;
        f.limit = 10;
        assert_eq!(list_events(&conn, &f).unwrap().1, 5);
        f.q = Some("E-5".into());
        assert_eq!(list_events(&conn, &f).unwrap().1, 1);
        let c = counts(&conn).unwrap();
        assert_eq!(c["active"], 4);
        assert_eq!(c["active_by_severity"]["critical"], 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn purge_by_retention_and_row_cap_keeps_open_events() {
        let (dir, mut conn) = db();
        let day = 86_400_000;
        let now = 1000 * day;
        let mut evs = Vec::new();
        for i in 0..10 {
            evs.push(event(
                &format!("E-old-{i}"),
                Severity::Critical,
                EventState::Resolved,
                now - 500 * day,
                Some(now - 450 * day),
            ));
        }
        for i in 0..10 {
            evs.push(event(
                &format!("E-new-{i}"),
                Severity::Critical,
                EventState::Resolved,
                now - 2 * day,
                Some(now - day + i),
            ));
        }
        evs.push(event(
            "E-open",
            Severity::Critical,
            EventState::Firing,
            now - 900 * day,
            None,
        ));
        let tx = conn.transaction().unwrap();
        upsert_events(&tx, &evs).unwrap();
        insert_timeline(
            &tx,
            &[crate::alerts::engine::Timeline {
                event_id: "E-old-1".into(),
                at: 1,
                actor: "t".into(),
                action: "fired",
                details: json!({}),
            }],
        )
        .unwrap();
        tx.commit().unwrap();
        // 保留 400 天：刪除 450 天前結案的 10 個與其處理紀錄
        let (p, more) = purge(&mut conn, now, 400, 1_000_000, 1000).unwrap();
        assert_eq!((p.events, p.timeline), (10, 1));
        assert!(!more);
        // 筆數上限 5：刪除最舊的已解決事件，進行中的不刪
        let (p, _) = purge(&mut conn, now, 400, 5, 1000).unwrap();
        assert_eq!(p.events, 6);
        let left: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT event_id FROM alert_events ORDER BY event_id")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert!(left.contains(&"E-open".to_string()));
        assert_eq!(left.len(), 5);
        assert!(left.contains(&"E-new-9".to_string()), "{left:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn config_rev_and_actions_queue() {
        let (dir, conn) = db();
        assert_eq!(config_rev(&conn).unwrap(), 0);
        bump_config_rev(&conn).unwrap();
        assert_eq!(config_rev(&conn).unwrap(), 1);
        let id = enqueue_action(&conn, 5, "ops", "E-1", "ack", &json!({})).unwrap();
        assert_eq!(pending_actions(&conn).unwrap().len(), 1);
        assert_eq!(action_result(&conn, id).unwrap(), None);
        finish_action(&conn, id, 6, &json!({"ok": true})).unwrap();
        assert!(pending_actions(&conn).unwrap().is_empty());
        assert_eq!(action_result(&conn, id).unwrap(), Some(json!({"ok": true})));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
