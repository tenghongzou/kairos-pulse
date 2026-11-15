//! [`crate::remote::store`] 的 SQLite 實作（[`RemoteStore`]）與 schema v9 的資料表。
// 方法參數與資料表欄位對應
#![allow(clippy::too_many_arguments)]

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::conn::{AsConn, forward};
use crate::remote::store::{
    ApplyingHost, ArtifactRow, AssignmentRow, ConfigReport, ConfigTarget, DesiredUpdate,
    HostConfigRow, HostTransition, NewRolloutHost, ProfileRevisionRow, ProfileRow, ReleaseRow,
    RemoteStore, RollbackFailure, RolloutHostCount, RolloutHostRow, RolloutRow, UpgradeHost,
};

/// schema v11（0.10.0）：升級推送的雙人覆核（REMOTE.md 第 5.4 節）。只新增欄位（`approved_at`／
/// `approved_by` v9 就有）。v10 是叢集協調（`store/sqlite/cluster.rs`）；PostgreSQL 的對等步驟在
/// `store/postgres/schema.rs` 的 `SCHEMA_V11`
pub(crate) const SCHEMA_V11_ROLLOUT_APPROVAL: &str = "
ALTER TABLE rollouts ADD COLUMN submitted_at INTEGER;
ALTER TABLE rollouts ADD COLUMN submitted_by TEXT;
ALTER TABLE rollouts ADD COLUMN reject_reason TEXT;
";

/// schema v12（0.11.0 起）：hello 的 `install.external_url`（REMOTE.md 第 2.1 節）——Pulse 宣告本機
/// 是否允許從外部 URL 下載（`none`／`allowlist`；NULL＝沒有宣告，較舊的 Pulse）。只新增一個可為 NULL 的
/// 欄位，不修改既有的資料。PostgreSQL 的對等步驟在 `store/postgres/schema.rs` 的 `SCHEMA_V12`
pub(crate) const SCHEMA_V12_INSTALL_EXTERNAL_URL: &str = "
ALTER TABLE hosts ADD COLUMN install_external_url TEXT;
";

/// schema v13（0.12.0 起）：hello 的 `install.rollback`（REMOTE.md 第 2.1 節；ADR-0003 階段 2）——
/// Pulse 宣告自動回滾的能力（`archived`／`binary_only`／`none`／`degraded`；NULL＝沒有宣告，較舊的 Pulse）。
/// 只新增一個可為 NULL 的欄位，不修改既有的資料。PostgreSQL 的對等步驟在 `store/postgres/schema.rs` 的
/// `SCHEMA_V13`
pub(crate) const SCHEMA_V13_INSTALL_ROLLBACK: &str = "
ALTER TABLE hosts ADD COLUMN install_rollback TEXT;
";

/// schema v9（0.9.0）：集中設定與 Pulse 遠端升級（商業審計 F8；`crates/kairos-proto/REMOTE.md`
/// 第 6 節、`kairos/STORAGE.md` 第 18 節）。只新增資料表與 `hosts` 的欄位，不修改既有的唯一鍵。
///
/// 與 REMOTE.md 第 6 節草案的差異：`host_config` 以 `agent_id` 為鍵（主機可能在清冊寫入之前就送出
/// `config_get`），另記最近一次 hello 的 `tags`、`agent_version`、`install.config`（清冊每 30 秒才同步，
/// 期望設定以 hello 的標籤計算）；主鍵欄位名稱沿用既有慣例（`profile_id`、`assignment_id`、`release_id`）；`releases`
/// 另有 `signer_principal`、`manifest_name`，`signer_fingerprint` 在沒有設定 `[remote] allowed_signers`
/// 時為 NULL；`release_artifacts` 另有 `stored_at`；`rollout_hosts` 另有 `acknowledged_at`／`_by`
/// （回滾失敗標記為已處理）；`rollouts` 另存 `target_version`、`release_id` 不設外鍵（rusqlite 內附的 SQLite
/// 預設開啟外鍵檢查；發佈版本刪除後已結束的推送紀錄仍要能顯示目標版本）。0.8 無法開啟 v9 的資料庫（降版從
/// 備份還原）。
pub(crate) const SCHEMA_V9: &str = "
CREATE TABLE config_profiles (
    profile_id  INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE COLLATE NOCASE,
    description TEXT NOT NULL DEFAULT '',
    settings    TEXT NOT NULL,
    revision    INTEGER NOT NULL DEFAULT 1,
    created_at  INTEGER NOT NULL,
    created_by  TEXT NOT NULL,
    updated_at  INTEGER NOT NULL,
    updated_by  TEXT NOT NULL
);

CREATE TABLE config_profile_revisions (
    profile_id  INTEGER NOT NULL REFERENCES config_profiles (profile_id) ON DELETE CASCADE,
    revision    INTEGER NOT NULL,
    settings    TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    changed_at  INTEGER NOT NULL,
    changed_by  TEXT NOT NULL,
    reason      TEXT,
    PRIMARY KEY (profile_id, revision)
) WITHOUT ROWID;

CREATE TABLE config_assignments (
    assignment_id  INTEGER PRIMARY KEY,
    profile_id     INTEGER NOT NULL REFERENCES config_profiles (profile_id),
    selector_kind  TEXT NOT NULL CHECK (selector_kind IN ('global', 'group', 'tag', 'host')),
    selector_value TEXT NOT NULL DEFAULT '',
    priority       INTEGER NOT NULL,
    expires_at     INTEGER,
    created_at     INTEGER NOT NULL,
    created_by     TEXT NOT NULL,
    UNIQUE (selector_kind, selector_value, profile_id)
);
CREATE INDEX config_assignments_profile ON config_assignments (profile_id);

CREATE TABLE host_config (
    agent_id     TEXT PRIMARY KEY,
    desired_rev  INTEGER NOT NULL DEFAULT 0,
    desired_hash TEXT NOT NULL DEFAULT '',
    desired      TEXT NOT NULL DEFAULT '{}',
    sources      TEXT NOT NULL DEFAULT '{}',
    applied_rev  INTEGER NOT NULL DEFAULT 0,
    applied_hash TEXT NOT NULL DEFAULT '',
    state        TEXT,
    effective    TEXT,
    overridden   TEXT,
    ignored      TEXT,
    errors       TEXT,
    code         TEXT,
    reported_at  INTEGER,
    tags          TEXT,
    agent_version TEXT,
    install_config TEXT
) WITHOUT ROWID;
CREATE INDEX host_config_state ON host_config (state);

ALTER TABLE hosts ADD COLUMN install_method TEXT;
ALTER TABLE hosts ADD COLUMN install_upgrade TEXT;
ALTER TABLE hosts ADD COLUMN install_config TEXT;

CREATE TABLE releases (
    release_id         INTEGER PRIMARY KEY,
    product            TEXT NOT NULL CHECK (product = 'pulse'),
    version            TEXT NOT NULL,
    commit_id          TEXT NOT NULL,
    source             TEXT NOT NULL CHECK (source IN ('hosted', 'external')),
    base_url           TEXT,
    sha256sums         TEXT NOT NULL,
    signature          TEXT NOT NULL,
    signer_fingerprint TEXT,
    signer_principal   TEXT,
    manifest_name      TEXT,
    manifest           TEXT,
    imported_at        INTEGER NOT NULL,
    imported_by        TEXT NOT NULL,
    UNIQUE (product, version)
);

CREATE TABLE release_artifacts (
    release_id INTEGER NOT NULL REFERENCES releases (release_id) ON DELETE CASCADE,
    filename   TEXT NOT NULL,
    os         TEXT NOT NULL,
    arch       TEXT NOT NULL,
    format     TEXT NOT NULL,
    sha256     TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    stored     INTEGER NOT NULL DEFAULT 0,
    stored_at  INTEGER,
    PRIMARY KEY (release_id, filename)
) WITHOUT ROWID;

CREATE TABLE rollouts (
    rollout_id        TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    release_id        INTEGER NOT NULL,
    target_version    TEXT NOT NULL,
    selector          TEXT NOT NULL,
    waves             TEXT NOT NULL,
    schedule          TEXT NOT NULL,
    options           TEXT NOT NULL,
    state             TEXT NOT NULL,
    current_wave      INTEGER NOT NULL DEFAULT 0,
    wave_started_at   INTEGER,
    soak_until        INTEGER,
    awaiting_continue INTEGER NOT NULL DEFAULT 0,
    pause_reason      TEXT,
    pause_cause       TEXT,
    cancel_reason     TEXT,
    created_at        INTEGER NOT NULL,
    created_by        TEXT NOT NULL,
    approved_at       INTEGER,
    approved_by       TEXT,
    started_at        INTEGER,
    completed_at      INTEGER,
    updated_at        INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX rollouts_state ON rollouts (state);

CREATE TABLE rollout_hosts (
    rollout_id      TEXT NOT NULL REFERENCES rollouts (rollout_id) ON DELETE CASCADE,
    host_id         INTEGER NOT NULL REFERENCES hosts (host_id),
    upgrade_id      TEXT NOT NULL UNIQUE,
    wave            INTEGER NOT NULL,
    ordinal         INTEGER NOT NULL,
    state           TEXT NOT NULL,
    reason          TEXT,
    from_version    TEXT,
    to_version      TEXT,
    attempts        INTEGER NOT NULL DEFAULT 0,
    bytes_done      INTEGER,
    bytes_total     INTEGER,
    code            TEXT,
    detail          TEXT,
    offered_at      INTEGER,
    applying_at     INTEGER,
    finished_at     INTEGER,
    updated_at      INTEGER NOT NULL,
    acknowledged_at INTEGER,
    acknowledged_by TEXT,
    PRIMARY KEY (rollout_id, host_id)
) WITHOUT ROWID;
CREATE INDEX rollout_hosts_host ON rollout_hosts (host_id, state);
CREATE INDEX rollout_hosts_state ON rollout_hosts (rollout_id, wave, state);
";

/// `meta` 的鍵：全域單調遞增的設定版本（REMOTE.md 第 3.4.4 節）
pub const META_CONFIG_REV: &str = "config_rev";

/// 推送已結束的狀態
const ROLLOUT_ENDED: &str = "('completed', 'cancelled')";

impl<C: AsConn> RemoteStore for C {
    forward! {
        fn remote_config_rev(&self) -> i64 => remote_config_rev;
        fn bump_remote_config_rev(&self) -> i64 => bump_remote_config_rev;
        fn list_profiles(&self) -> Vec<ProfileRow> => list_profiles;
        fn get_profile(&self, id: i64) -> Option<ProfileRow> => get_profile;
        fn profile_id_by_name(&self, name: &str) -> Option<i64> => profile_id_by_name;
        fn insert_profile(&self, name: &str, description: &str, settings: &str, now_ms: i64, by: &str, reason: Option<&str>) -> i64 => insert_profile;
        fn update_profile(&self, id: i64, name: &str, description: &str, settings: &str, now_ms: i64, by: &str, reason: Option<&str>) -> i64 => update_profile;
        fn delete_profile(&self, id: i64) -> bool => delete_profile;
        fn profile_revisions(&self, id: i64) -> Vec<ProfileRevisionRow> => profile_revisions;
        fn list_assignments(&self) -> Vec<AssignmentRow> => list_assignments;
        fn get_assignment(&self, id: i64) -> Option<AssignmentRow> => get_assignment;
        fn insert_assignment(&self, profile_id: i64, selector_kind: &str, selector_value: &str, priority: i64, expires_at: Option<i64>, now_ms: i64, by: &str) -> Option<i64> => insert_assignment;
        fn delete_assignment(&self, id: i64) -> bool => delete_assignment;
        fn expired_assignments(&self, now_ms: i64) -> Vec<AssignmentRow> => expired_assignments;
        fn config_targets(&self) -> Vec<ConfigTarget> => config_targets;
        fn config_target(&self, agent_id: &str) -> Option<ConfigTarget> => config_target;
        fn desired_all(&self) -> Vec<(String, i64, String)> => desired_all;
        fn host_config(&self, agent_id: &str) -> Option<HostConfigRow> => host_config;
        fn host_configs(&self) -> Vec<HostConfigRow> => host_configs;
        fn upsert_desired(&self, rows: &[DesiredUpdate]) -> () => upsert_desired;
        fn upsert_hello_info(&self, agent_id: &str, tags: &str, agent_version: &str, install_config: Option<&str>) -> () => upsert_hello_info;
        fn record_config_status(&self, r: &ConfigReport) -> (Option<String>, usize) => record_config_status;
        fn list_releases(&self) -> Vec<ReleaseRow> => list_releases;
        fn get_release(&self, version: &str) -> Option<ReleaseRow> => get_release;
        fn get_release_by_id(&self, id: i64) -> Option<ReleaseRow> => get_release_by_id;
        fn insert_release(&self, r: &ReleaseRow) -> i64 => insert_release;
        fn delete_release(&self, id: i64) -> bool => delete_release;
        fn release_artifacts(&self, release_id: i64) -> Vec<ArtifactRow> => release_artifacts;
        fn all_artifacts(&self) -> Vec<ArtifactRow> => all_artifacts;
        fn upsert_artifact(&self, a: &ArtifactRow) -> () => upsert_artifact;
        fn artifact_refs(&self, sha256: &str) -> i64 => artifact_refs;
        fn upgrade_hosts(&self) -> Vec<UpgradeHost> => upgrade_hosts;
        fn upgrade_host(&self, agent_id: &str) -> Option<UpgradeHost> => upgrade_host;
        fn list_rollouts(&self, state: Option<&str>) -> Vec<RolloutRow> => list_rollouts;
        fn get_rollout(&self, id: &str) -> Option<RolloutRow> => get_rollout;
        fn count_rollouts_with_prefix(&self, prefix: &str) -> i64 => count_rollouts_with_prefix;
        fn insert_rollout(&self, r: &RolloutRow) -> () => insert_rollout;
        fn update_rollout_spec(&self, id: &str, name: &str, release_id: i64, selector: &str, waves: &str, options: &str, now_ms: i64) -> bool => update_rollout_spec;
        fn update_rollout_schedule(&self, id: &str, schedule: &str, now_ms: i64) -> bool => update_rollout_schedule;
        fn update_rollout_state(&self, r: &RolloutRow) -> () => update_rollout_state;
        fn delete_rollout_hosts(&self, rollout_id: &str) -> i64 => delete_rollout_hosts;
        fn delete_old_rollouts(&self, before_ms: i64) -> i64 => delete_old_rollouts;
        fn insert_rollout_hosts(&self, rollout_id: &str, hosts: &[NewRolloutHost], now_ms: i64) -> () => insert_rollout_hosts;
        fn rollout_hosts(&self, rollout_id: &str) -> Vec<RolloutHostRow> => rollout_hosts;
        fn rollout_host_by_upgrade_id(&self, upgrade_id: &str) -> Option<RolloutHostRow> => rollout_host_by_upgrade_id;
        fn rollout_host(&self, rollout_id: &str, agent_id: &str) -> Option<RolloutHostRow> => rollout_host;
        fn host_upgrades(&self, agent_id: &str, limit: i64) -> Vec<RolloutHostRow> => host_upgrades;
        fn update_rollout_host(&self, rollout_id: &str, host_id: i64, t: &HostTransition, now_ms: i64) -> () => update_rollout_host;
        fn hosts_in_active_rollouts(&self) -> Vec<(String, String)> => hosts_in_active_rollouts;
        fn active_offers(&self) -> Vec<(String, String)> => active_offers;
        fn unacknowledged_rollback_failures(&self) -> Vec<RollbackFailure> => unacknowledged_rollback_failures;
        fn applying_hosts(&self) -> Vec<ApplyingHost> => applying_hosts;
        fn rollouts_using_release(&self, release_id: i64) -> i64 => rollouts_using_release;
        fn rollout_host_counts(&self, state: Option<&str>) -> Vec<RolloutHostCount> => rollout_host_counts;
        fn rollout_state_counts(&self) -> Vec<(String, i64)> => rollout_state_counts;
        fn count_unacknowledged_rollback_failures(&self) -> i64 => count_unacknowledged_rollback_failures;
    }
}

// ---- 設定版本 ----

pub fn remote_config_rev(conn: &Connection) -> rusqlite::Result<i64> {
    Ok(super::alerts::get_meta(conn, META_CONFIG_REV)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0))
}

pub fn bump_remote_config_rev(conn: &Connection) -> rusqlite::Result<i64> {
    let next = remote_config_rev(conn)? + 1;
    super::alerts::put_meta(conn, META_CONFIG_REV, &next.to_string())?;
    Ok(next)
}

// ---- 設定檔 ----

const PROFILE_COLUMNS: &str = "profile_id, name, description, settings, revision, created_at, \
                               created_by, updated_at, updated_by";

fn profile_of(r: &Row<'_>) -> rusqlite::Result<ProfileRow> {
    Ok(ProfileRow {
        id: r.get(0)?,
        name: r.get(1)?,
        description: r.get(2)?,
        settings: r.get(3)?,
        revision: r.get(4)?,
        created_at: r.get(5)?,
        created_by: r.get(6)?,
        updated_at: r.get(7)?,
        updated_by: r.get(8)?,
    })
}

pub fn list_profiles(conn: &Connection) -> rusqlite::Result<Vec<ProfileRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PROFILE_COLUMNS} FROM config_profiles ORDER BY name COLLATE NOCASE, profile_id"
    ))?;
    let rows = stmt.query_map([], profile_of)?;
    rows.collect()
}

pub fn get_profile(conn: &Connection, id: i64) -> rusqlite::Result<Option<ProfileRow>> {
    conn.query_row(
        &format!("SELECT {PROFILE_COLUMNS} FROM config_profiles WHERE profile_id = ?1"),
        [id],
        profile_of,
    )
    .optional()
}

pub fn profile_id_by_name(conn: &Connection, name: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT profile_id FROM config_profiles WHERE name = ?1 COLLATE NOCASE",
        [name],
        |r| r.get(0),
    )
    .optional()
}

pub fn insert_profile(
    conn: &Connection,
    name: &str,
    description: &str,
    settings: &str,
    now_ms: i64,
    by: &str,
    reason: Option<&str>,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO config_profiles (name, description, settings, revision, created_at, created_by,
            updated_at, updated_by)
         VALUES (?1, ?2, ?3, 1, ?4, ?5, ?4, ?5)",
        params![name, description, settings, now_ms, by],
    )?;
    let id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO config_profile_revisions (profile_id, revision, settings, description,
            changed_at, changed_by, reason)
         VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6)",
        params![id, settings, description, now_ms, by, reason],
    )?;
    Ok(id)
}

pub fn update_profile(
    conn: &Connection,
    id: i64,
    name: &str,
    description: &str,
    settings: &str,
    now_ms: i64,
    by: &str,
    reason: Option<&str>,
) -> rusqlite::Result<i64> {
    let revision: i64 = conn.query_row(
        "UPDATE config_profiles SET name = ?2, description = ?3, settings = ?4,
            revision = revision + 1, updated_at = ?5, updated_by = ?6
         WHERE profile_id = ?1 RETURNING revision",
        params![id, name, description, settings, now_ms, by],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO config_profile_revisions (profile_id, revision, settings, description,
            changed_at, changed_by, reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, revision, settings, description, now_ms, by, reason],
    )?;
    Ok(revision)
}

pub fn delete_profile(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    conn.execute(
        "DELETE FROM config_profile_revisions WHERE profile_id = ?1",
        [id],
    )?;
    Ok(conn.execute("DELETE FROM config_profiles WHERE profile_id = ?1", [id])? > 0)
}

pub fn profile_revisions(conn: &Connection, id: i64) -> rusqlite::Result<Vec<ProfileRevisionRow>> {
    let mut stmt = conn.prepare(
        "SELECT revision, settings, description, changed_at, changed_by, reason
         FROM config_profile_revisions WHERE profile_id = ?1 ORDER BY revision DESC",
    )?;
    let rows = stmt.query_map([id], |r| {
        Ok(ProfileRevisionRow {
            revision: r.get(0)?,
            settings: r.get(1)?,
            description: r.get(2)?,
            changed_at: r.get(3)?,
            changed_by: r.get(4)?,
            reason: r.get(5)?,
        })
    })?;
    rows.collect()
}

// ---- 指派 ----

const ASSIGNMENT_COLUMNS: &str = "assignment_id, profile_id, selector_kind, selector_value, \
                                  priority, expires_at, created_at, created_by";

fn assignment_of(r: &Row<'_>) -> rusqlite::Result<AssignmentRow> {
    Ok(AssignmentRow {
        id: r.get(0)?,
        profile_id: r.get(1)?,
        selector_kind: r.get(2)?,
        selector_value: r.get(3)?,
        priority: r.get(4)?,
        expires_at: r.get(5)?,
        created_at: r.get(6)?,
        created_by: r.get(7)?,
    })
}

pub fn list_assignments(conn: &Connection) -> rusqlite::Result<Vec<AssignmentRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ASSIGNMENT_COLUMNS} FROM config_assignments ORDER BY priority, assignment_id"
    ))?;
    let rows = stmt.query_map([], assignment_of)?;
    rows.collect()
}

pub fn get_assignment(conn: &Connection, id: i64) -> rusqlite::Result<Option<AssignmentRow>> {
    conn.query_row(
        &format!("SELECT {ASSIGNMENT_COLUMNS} FROM config_assignments WHERE assignment_id = ?1"),
        [id],
        assignment_of,
    )
    .optional()
}

pub fn insert_assignment(
    conn: &Connection,
    profile_id: i64,
    selector_kind: &str,
    selector_value: &str,
    priority: i64,
    expires_at: Option<i64>,
    now_ms: i64,
    by: &str,
) -> rusqlite::Result<Option<i64>> {
    let n = conn.execute(
        "INSERT INTO config_assignments (profile_id, selector_kind, selector_value, priority,
            expires_at, created_at, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (selector_kind, selector_value, profile_id) DO NOTHING",
        params![
            profile_id,
            selector_kind,
            selector_value,
            priority,
            expires_at,
            now_ms,
            by
        ],
    )?;
    Ok((n > 0).then(|| conn.last_insert_rowid()))
}

pub fn delete_assignment(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM config_assignments WHERE assignment_id = ?1",
        [id],
    )? > 0)
}

pub fn expired_assignments(conn: &Connection, now_ms: i64) -> rusqlite::Result<Vec<AssignmentRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ASSIGNMENT_COLUMNS} FROM config_assignments
         WHERE expires_at IS NOT NULL AND expires_at <= ?1 ORDER BY assignment_id"
    ))?;
    let rows = stmt.query_map([now_ms], assignment_of)?;
    rows.collect()
}

// ---- 每台主機的設定 ----

fn parse_tags(text: Option<String>) -> Vec<String> {
    text.and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
        .unwrap_or_default()
}

fn target_of(r: &Row<'_>) -> rusqlite::Result<ConfigTarget> {
    Ok(ConfigTarget {
        agent_id: r.get(0)?,
        host_group: r.get(1)?,
        tags: parse_tags(r.get(2)?),
        agent_version: r.get(3)?,
        install_config: r.get(4)?,
    })
}

const TARGETS: &str = "SELECT h.agent_id, h.host_group, coalesce(hc.tags, h.tags),
        coalesce(hc.agent_version, h.agent_version), coalesce(hc.install_config, h.install_config)
     FROM hosts h LEFT JOIN host_config hc ON hc.agent_id = h.agent_id
     WHERE h.lifecycle = 'active'
     UNION ALL
     SELECT agent_id, NULL, tags, agent_version, install_config FROM host_config
     WHERE agent_id NOT IN (SELECT agent_id FROM hosts)";

pub fn config_targets(conn: &Connection) -> rusqlite::Result<Vec<ConfigTarget>> {
    let mut stmt = conn.prepare(TARGETS)?;
    let rows = stmt.query_map([], target_of)?;
    rows.collect()
}

pub fn config_target(conn: &Connection, agent_id: &str) -> rusqlite::Result<Option<ConfigTarget>> {
    conn.query_row(
        &format!("SELECT * FROM ({TARGETS}) WHERE agent_id = ?1"),
        [agent_id],
        target_of,
    )
    .optional()
}

pub fn desired_all(conn: &Connection) -> rusqlite::Result<Vec<(String, i64, String)>> {
    let mut stmt = conn.prepare("SELECT agent_id, desired_rev, desired_hash FROM host_config")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    rows.collect()
}

const HOST_CONFIG_COLUMNS: &str = "agent_id, desired_rev, desired_hash, desired, sources, \
     applied_rev, applied_hash, state, effective, overridden, ignored, errors, code, reported_at, \
     tags, agent_version, install_config";

fn host_config_of(r: &Row<'_>) -> rusqlite::Result<HostConfigRow> {
    Ok(HostConfigRow {
        agent_id: r.get(0)?,
        desired_rev: r.get(1)?,
        desired_hash: r.get(2)?,
        desired: r.get(3)?,
        sources: r.get(4)?,
        applied_rev: r.get(5)?,
        applied_hash: r.get(6)?,
        state: r.get(7)?,
        effective: r.get(8)?,
        overridden: r.get(9)?,
        ignored: r.get(10)?,
        errors: r.get(11)?,
        code: r.get(12)?,
        reported_at: r.get(13)?,
        tags: r.get(14)?,
        agent_version: r.get(15)?,
        install_config: r.get(16)?,
    })
}

pub fn host_config(conn: &Connection, agent_id: &str) -> rusqlite::Result<Option<HostConfigRow>> {
    conn.query_row(
        &format!("SELECT {HOST_CONFIG_COLUMNS} FROM host_config WHERE agent_id = ?1"),
        [agent_id],
        host_config_of,
    )
    .optional()
}

pub fn host_configs(conn: &Connection) -> rusqlite::Result<Vec<HostConfigRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {HOST_CONFIG_COLUMNS} FROM host_config"))?;
    let rows = stmt.query_map([], host_config_of)?;
    rows.collect()
}

pub fn upsert_desired(conn: &Connection, rows: &[DesiredUpdate]) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO host_config (agent_id, desired_rev, desired_hash, desired, sources)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (agent_id) DO UPDATE SET
            desired_rev = excluded.desired_rev, desired_hash = excluded.desired_hash,
            desired = excluded.desired, sources = excluded.sources",
    )?;
    for r in rows {
        stmt.execute(params![
            r.agent_id,
            r.desired_rev,
            r.desired_hash,
            r.desired,
            r.sources
        ])?;
    }
    Ok(())
}

pub fn upsert_hello_info(
    conn: &Connection,
    agent_id: &str,
    tags: &str,
    agent_version: &str,
    install_config: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO host_config (agent_id, tags, agent_version, install_config) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (agent_id) DO UPDATE SET tags = excluded.tags,
            agent_version = excluded.agent_version, install_config = excluded.install_config",
        params![agent_id, tags, agent_version, install_config],
    )?;
    Ok(())
}

pub fn record_config_status(
    conn: &Connection,
    r: &ConfigReport,
) -> rusqlite::Result<(Option<String>, usize)> {
    let before: Option<(i64, i64, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT applied_rev, desired_rev, state, overridden FROM host_config WHERE agent_id = ?1",
            [&r.agent_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let (applied_rev, desired_rev, prev_state, prev_overridden) =
        before.unwrap_or((0, 0, None, None));
    let prev_count = prev_overridden
        .and_then(|t| serde_json::from_str::<Vec<serde_json::Value>>(&t).ok())
        .map_or(0, |v| v.len());
    conn.execute(
        "INSERT INTO host_config (agent_id) VALUES (?1) ON CONFLICT (agent_id) DO NOTHING",
        [&r.agent_id],
    )?;
    // 比已記錄的舊、又不是目前的期望版本：晚到的回報，不倒退版本。期望設定回到第 0 版（刪除所有指派）時
    // Pulse 回報的 rev 0 比已記錄的小，但就是目前的期望版本，照常記錄
    if r.rev < applied_rev && r.rev != desired_rev {
        conn.execute(
            "UPDATE host_config SET effective = ?2, overridden = ?3, reported_at = ?4
             WHERE agent_id = ?1",
            params![r.agent_id, r.effective, r.overridden, r.reported_at],
        )?;
    } else {
        conn.execute(
            "UPDATE host_config SET applied_rev = ?2, applied_hash = ?3, state = ?4, effective = ?5,
                overridden = ?6, ignored = ?7, errors = ?8, code = ?9, reported_at = ?10
             WHERE agent_id = ?1",
            params![
                r.agent_id,
                r.rev,
                r.hash,
                r.state,
                r.effective,
                r.overridden,
                r.ignored,
                r.errors,
                r.code,
                r.reported_at
            ],
        )?;
    }
    Ok((prev_state, prev_count))
}

// ---- 版本庫 ----

const RELEASE_COLUMNS: &str = "release_id, version, commit_id, source, sha256sums, signature, \
     signer_fingerprint, signer_principal, manifest_name, manifest, imported_at, imported_by, \
     base_url";

fn release_of(r: &Row<'_>) -> rusqlite::Result<ReleaseRow> {
    Ok(ReleaseRow {
        id: r.get(0)?,
        version: r.get(1)?,
        commit: r.get(2)?,
        source: r.get(3)?,
        sha256sums: r.get(4)?,
        signature: r.get(5)?,
        signer_fingerprint: r.get(6)?,
        signer_principal: r.get(7)?,
        manifest_name: r.get(8)?,
        manifest: r.get(9)?,
        imported_at: r.get(10)?,
        imported_by: r.get(11)?,
        base_url: r.get(12)?,
    })
}

pub fn list_releases(conn: &Connection) -> rusqlite::Result<Vec<ReleaseRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RELEASE_COLUMNS} FROM releases ORDER BY imported_at DESC, release_id DESC"
    ))?;
    let rows = stmt.query_map([], release_of)?;
    rows.collect()
}

pub fn get_release(conn: &Connection, version: &str) -> rusqlite::Result<Option<ReleaseRow>> {
    conn.query_row(
        &format!("SELECT {RELEASE_COLUMNS} FROM releases WHERE product = 'pulse' AND version = ?1"),
        [version],
        release_of,
    )
    .optional()
}

pub fn get_release_by_id(conn: &Connection, id: i64) -> rusqlite::Result<Option<ReleaseRow>> {
    conn.query_row(
        &format!("SELECT {RELEASE_COLUMNS} FROM releases WHERE release_id = ?1"),
        [id],
        release_of,
    )
    .optional()
}

pub fn insert_release(conn: &Connection, r: &ReleaseRow) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO releases (product, version, commit_id, source, sha256sums, signature,
            signer_fingerprint, signer_principal, manifest_name, manifest, imported_at, imported_by,
            base_url)
         VALUES ('pulse', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            r.version,
            r.commit,
            r.source,
            r.sha256sums,
            r.signature,
            r.signer_fingerprint,
            r.signer_principal,
            r.manifest_name,
            r.manifest,
            r.imported_at,
            r.imported_by,
            r.base_url
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn delete_release(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    conn.execute("DELETE FROM release_artifacts WHERE release_id = ?1", [id])?;
    Ok(conn.execute("DELETE FROM releases WHERE release_id = ?1", [id])? > 0)
}

const ARTIFACT_COLUMNS: &str =
    "release_id, filename, os, arch, format, sha256, size_bytes, stored, stored_at";

fn artifact_of(r: &Row<'_>) -> rusqlite::Result<ArtifactRow> {
    Ok(ArtifactRow {
        release_id: r.get(0)?,
        filename: r.get(1)?,
        os: r.get(2)?,
        arch: r.get(3)?,
        format: r.get(4)?,
        sha256: r.get(5)?,
        size: r.get(6)?,
        stored: r.get::<_, i64>(7)? != 0,
        stored_at: r.get(8)?,
    })
}

pub fn release_artifacts(conn: &Connection, release_id: i64) -> rusqlite::Result<Vec<ArtifactRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ARTIFACT_COLUMNS} FROM release_artifacts WHERE release_id = ?1 ORDER BY filename"
    ))?;
    let rows = stmt.query_map([release_id], artifact_of)?;
    rows.collect()
}

pub fn all_artifacts(conn: &Connection) -> rusqlite::Result<Vec<ArtifactRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ARTIFACT_COLUMNS} FROM release_artifacts ORDER BY release_id, filename"
    ))?;
    let rows = stmt.query_map([], artifact_of)?;
    rows.collect()
}

pub fn upsert_artifact(conn: &Connection, a: &ArtifactRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO release_artifacts (release_id, filename, os, arch, format, sha256, size_bytes,
            stored, stored_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (release_id, filename) DO UPDATE SET
            size_bytes = excluded.size_bytes, stored = excluded.stored, stored_at = excluded.stored_at",
        params![
            a.release_id,
            a.filename,
            a.os,
            a.arch,
            a.format,
            a.sha256,
            a.size,
            a.stored as i64,
            a.stored_at
        ],
    )?;
    Ok(())
}

pub fn artifact_refs(conn: &Connection, sha256: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM release_artifacts WHERE sha256 = ?1 AND stored = 1",
        [sha256],
        |r| r.get(0),
    )
}

// ---- 推送 ----

const UPGRADE_HOST_COLUMNS: &str = "host_id, agent_id, hostname, agent_version, os, install_method, \
     install_upgrade, install_config, host_group, tags, online, lifecycle, install_external_url, \
     install_rollback";

fn upgrade_host_of(r: &Row<'_>) -> rusqlite::Result<UpgradeHost> {
    Ok(UpgradeHost {
        host_id: r.get(0)?,
        agent_id: r.get(1)?,
        hostname: r.get(2)?,
        agent_version: r.get(3)?,
        os: r.get(4)?,
        install_method: r.get(5)?,
        install_upgrade: r.get(6)?,
        install_config: r.get(7)?,
        host_group: r.get(8)?,
        tags: parse_tags(r.get(9)?),
        online: r.get::<_, i64>(10)? != 0,
        decommissioned: r.get::<_, String>(11)? != "active",
        install_external_url: r.get(12)?,
        install_rollback: r.get(13)?,
    })
}

pub fn upgrade_hosts(conn: &Connection) -> rusqlite::Result<Vec<UpgradeHost>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {UPGRADE_HOST_COLUMNS} FROM hosts ORDER BY host_id"
    ))?;
    let rows = stmt.query_map([], upgrade_host_of)?;
    rows.collect()
}

pub fn upgrade_host(conn: &Connection, agent_id: &str) -> rusqlite::Result<Option<UpgradeHost>> {
    conn.query_row(
        &format!("SELECT {UPGRADE_HOST_COLUMNS} FROM hosts WHERE agent_id = ?1"),
        [agent_id],
        upgrade_host_of,
    )
    .optional()
}

const ROLLOUT_COLUMNS: &str = "r.rollout_id, r.name, r.release_id, r.target_version, r.selector, r.waves, \
     r.schedule, r.options, r.state, r.current_wave, r.wave_started_at, r.soak_until, \
     r.awaiting_continue, r.pause_reason, r.cancel_reason, r.created_at, r.created_by, r.started_at, \
     r.completed_at, r.updated_at, r.pause_cause, r.submitted_at, r.submitted_by, r.approved_at,      r.approved_by, r.reject_reason";

fn rollout_of(r: &Row<'_>) -> rusqlite::Result<RolloutRow> {
    Ok(RolloutRow {
        id: r.get(0)?,
        name: r.get(1)?,
        release_id: r.get(2)?,
        target_version: r.get(3)?,
        selector: r.get(4)?,
        waves: r.get(5)?,
        schedule: r.get(6)?,
        options: r.get(7)?,
        state: r.get(8)?,
        current_wave: r.get(9)?,
        wave_started_at: r.get(10)?,
        soak_until: r.get(11)?,
        awaiting_continue: r.get::<_, i64>(12)? != 0,
        pause_reason: r.get(13)?,
        pause_cause: r.get(20)?,
        cancel_reason: r.get(14)?,
        created_at: r.get(15)?,
        created_by: r.get(16)?,
        started_at: r.get(17)?,
        completed_at: r.get(18)?,
        updated_at: r.get(19)?,
        submitted_at: r.get(21)?,
        submitted_by: r.get(22)?,
        approved_at: r.get(23)?,
        approved_by: r.get(24)?,
        reject_reason: r.get(25)?,
    })
}

pub fn list_rollouts(conn: &Connection, state: Option<&str>) -> rusqlite::Result<Vec<RolloutRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ROLLOUT_COLUMNS} FROM rollouts r
         WHERE ?1 IS NULL OR r.state = ?1 ORDER BY r.created_at DESC, r.rollout_id DESC"
    ))?;
    let rows = stmt.query_map([state], rollout_of)?;
    rows.collect()
}

pub fn get_rollout(conn: &Connection, id: &str) -> rusqlite::Result<Option<RolloutRow>> {
    conn.query_row(
        &format!("SELECT {ROLLOUT_COLUMNS} FROM rollouts r WHERE r.rollout_id = ?1"),
        [id],
        rollout_of,
    )
    .optional()
}

pub fn count_rollouts_with_prefix(conn: &Connection, prefix: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM rollouts WHERE substr(rollout_id, 1, length(?1)) = ?1",
        [prefix],
        |r| r.get(0),
    )
}

pub fn insert_rollout(conn: &Connection, r: &RolloutRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO rollouts (rollout_id, name, release_id, selector, waves, schedule, options, state,
            current_wave, wave_started_at, soak_until, awaiting_continue, pause_reason, cancel_reason,
            created_at, created_by, started_at, completed_at, updated_at, target_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
        params![
            r.id,
            r.name,
            r.release_id,
            r.selector,
            r.waves,
            r.schedule,
            r.options,
            r.state,
            r.current_wave,
            r.wave_started_at,
            r.soak_until,
            r.awaiting_continue as i64,
            r.pause_reason,
            r.cancel_reason,
            r.created_at,
            r.created_by,
            r.started_at,
            r.completed_at,
            r.updated_at,
            r.target_version
        ],
    )?;
    Ok(())
}

pub fn update_rollout_spec(
    conn: &Connection,
    id: &str,
    name: &str,
    release_id: i64,
    selector: &str,
    waves: &str,
    options: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE rollouts SET name = ?2, release_id = ?3, selector = ?4, waves = ?5, options = ?6,
            updated_at = ?7,
            target_version = coalesce((SELECT version FROM releases WHERE release_id = ?3), target_version)
         WHERE rollout_id = ?1 AND state = 'draft'",
        params![id, name, release_id, selector, waves, options, now_ms],
    )? > 0)
}

pub fn update_rollout_schedule(
    conn: &Connection,
    id: &str,
    schedule: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE rollouts SET schedule = ?2, updated_at = ?3
         WHERE rollout_id = ?1 AND state = 'draft'",
        params![id, schedule, now_ms],
    )? > 0)
}

pub fn delete_rollout_hosts(conn: &Connection, rollout_id: &str) -> rusqlite::Result<i64> {
    Ok(conn.execute(
        "DELETE FROM rollout_hosts WHERE rollout_id = ?1",
        [rollout_id],
    )? as i64)
}

pub fn update_rollout_state(conn: &Connection, r: &RolloutRow) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE rollouts SET state = ?2, current_wave = ?3, wave_started_at = ?4, soak_until = ?5,
            awaiting_continue = ?6, pause_reason = ?7, cancel_reason = ?8, started_at = ?9,
            pause_cause = ?12,
            completed_at = ?10, updated_at = ?11,
            submitted_at = ?13, submitted_by = ?14, approved_at = ?15, approved_by = ?16,
            reject_reason = ?17
         WHERE rollout_id = ?1",
        params![
            r.id,
            r.state,
            r.current_wave,
            r.wave_started_at,
            r.soak_until,
            r.awaiting_continue as i64,
            r.pause_reason,
            r.cancel_reason,
            r.started_at,
            r.completed_at,
            r.updated_at,
            r.pause_cause,
            r.submitted_at,
            r.submitted_by,
            r.approved_at,
            r.approved_by,
            r.reject_reason
        ],
    )?;
    Ok(())
}

pub fn delete_old_rollouts(conn: &Connection, before_ms: i64) -> rusqlite::Result<i64> {
    let ids = format!(
        "SELECT rollout_id FROM rollouts WHERE state IN {ROLLOUT_ENDED}
         AND coalesce(completed_at, updated_at) < ?1"
    );
    conn.execute(
        &format!("DELETE FROM rollout_hosts WHERE rollout_id IN ({ids})"),
        [before_ms],
    )?;
    Ok(conn.execute(
        &format!("DELETE FROM rollouts WHERE rollout_id IN ({ids})"),
        [before_ms],
    )? as i64)
}

pub fn insert_rollout_hosts(
    conn: &Connection,
    rollout_id: &str,
    hosts: &[NewRolloutHost],
    now_ms: i64,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO rollout_hosts (rollout_id, host_id, upgrade_id, wave, ordinal, state, reason,
            from_version, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for h in hosts {
        stmt.execute(params![
            rollout_id,
            h.host_id,
            h.upgrade_id,
            h.wave,
            h.ordinal,
            h.state,
            h.reason,
            h.from_version,
            now_ms
        ])?;
    }
    Ok(())
}

const ROLLOUT_HOST_COLUMNS: &str = "rh.rollout_id, rh.host_id, coalesce(h.agent_id, ''), \
     coalesce(h.hostname, ''), rh.upgrade_id, rh.wave, rh.ordinal, rh.state, rh.reason, \
     rh.from_version, rh.to_version, rh.attempts, rh.bytes_done, rh.bytes_total, rh.code, rh.detail, \
     rh.offered_at, rh.applying_at, rh.finished_at, rh.updated_at, rh.acknowledged_at, \
     rh.acknowledged_by";

const ROLLOUT_HOST_FROM: &str = "rollout_hosts rh LEFT JOIN hosts h ON h.host_id = rh.host_id";

fn rollout_host_of(r: &Row<'_>) -> rusqlite::Result<RolloutHostRow> {
    Ok(RolloutHostRow {
        rollout_id: r.get(0)?,
        host_id: r.get(1)?,
        agent_id: r.get(2)?,
        hostname: r.get(3)?,
        upgrade_id: r.get(4)?,
        wave: r.get(5)?,
        ordinal: r.get(6)?,
        state: r.get(7)?,
        reason: r.get(8)?,
        from_version: r.get(9)?,
        to_version: r.get(10)?,
        attempts: r.get(11)?,
        bytes_done: r.get(12)?,
        bytes_total: r.get(13)?,
        code: r.get(14)?,
        detail: r.get(15)?,
        offered_at: r.get(16)?,
        applying_at: r.get(17)?,
        finished_at: r.get(18)?,
        updated_at: r.get(19)?,
        acknowledged_at: r.get(20)?,
        acknowledged_by: r.get(21)?,
    })
}

pub fn rollout_hosts(conn: &Connection, rollout_id: &str) -> rusqlite::Result<Vec<RolloutHostRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ROLLOUT_HOST_COLUMNS} FROM {ROLLOUT_HOST_FROM}
         WHERE rh.rollout_id = ?1 ORDER BY rh.wave, rh.ordinal"
    ))?;
    let rows = stmt.query_map([rollout_id], rollout_host_of)?;
    rows.collect()
}

pub fn rollout_host_by_upgrade_id(
    conn: &Connection,
    upgrade_id: &str,
) -> rusqlite::Result<Option<RolloutHostRow>> {
    conn.query_row(
        &format!("SELECT {ROLLOUT_HOST_COLUMNS} FROM {ROLLOUT_HOST_FROM} WHERE rh.upgrade_id = ?1"),
        [upgrade_id],
        rollout_host_of,
    )
    .optional()
}

pub fn rollout_host(
    conn: &Connection,
    rollout_id: &str,
    agent_id: &str,
) -> rusqlite::Result<Option<RolloutHostRow>> {
    conn.query_row(
        &format!(
            "SELECT {ROLLOUT_HOST_COLUMNS} FROM {ROLLOUT_HOST_FROM}
             WHERE rh.rollout_id = ?1 AND h.agent_id = ?2"
        ),
        [rollout_id, agent_id],
        rollout_host_of,
    )
    .optional()
}

pub fn host_upgrades(
    conn: &Connection,
    agent_id: &str,
    limit: i64,
) -> rusqlite::Result<Vec<RolloutHostRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ROLLOUT_HOST_COLUMNS} FROM {ROLLOUT_HOST_FROM}
         WHERE h.agent_id = ?1 ORDER BY rh.updated_at DESC, rh.rollout_id DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![agent_id, limit], rollout_host_of)?;
    rows.collect()
}

pub fn update_rollout_host(
    conn: &Connection,
    rollout_id: &str,
    host_id: i64,
    t: &HostTransition,
    now_ms: i64,
) -> rusqlite::Result<()> {
    // 只更新有給的欄位：以 coalesce／CASE 組出一個 UPDATE（欄位數固定，可以快取 statement）
    let flag = |b: bool| b as i64;
    let mut stmt = conn.prepare(
        "UPDATE rollout_hosts SET
            state = coalesce(?3, state),
            reason = CASE WHEN ?4 THEN ?5 ELSE reason END,
            upgrade_id = coalesce(?6, upgrade_id),
            from_version = coalesce(?7, from_version),
            to_version = coalesce(?8, to_version),
            attempts = attempts + ?9,
            bytes_done = coalesce(?10, bytes_done),
            bytes_total = coalesce(?11, bytes_total),
            code = CASE WHEN ?12 THEN ?13 ELSE code END,
            detail = CASE WHEN ?14 THEN ?15 ELSE detail END,
            offered_at = CASE WHEN ?16 THEN ?17 ELSE offered_at END,
            applying_at = CASE WHEN ?18 THEN ?19 ELSE applying_at END,
            finished_at = CASE WHEN ?20 THEN ?21 ELSE finished_at END,
            acknowledged_at = CASE WHEN ?22 THEN ?23 ELSE acknowledged_at END,
            acknowledged_by = CASE WHEN ?22 THEN ?24 ELSE acknowledged_by END,
            updated_at = ?25
         WHERE rollout_id = ?1 AND host_id = ?2",
    )?;
    let (ack_at, ack_by) = match &t.acknowledged {
        Some((at, by)) => (Some(*at), Some(by.as_str())),
        None => (None, None),
    };
    stmt.execute(params![
        rollout_id,
        host_id,
        t.state,
        flag(t.reason.is_some()),
        t.reason.clone().flatten(),
        t.upgrade_id,
        t.from_version,
        t.to_version,
        flag(t.bump_attempts),
        t.bytes_done,
        t.bytes_total,
        flag(t.code.is_some()),
        t.code.clone().flatten(),
        flag(t.detail.is_some()),
        t.detail.clone().flatten(),
        flag(t.offered_at.is_some()),
        t.offered_at.flatten(),
        flag(t.applying_at.is_some()),
        t.applying_at.flatten(),
        flag(t.finished_at.is_some()),
        t.finished_at.flatten(),
        flag(t.acknowledged.is_some()),
        ack_at,
        ack_by,
        now_ms
    ])?;
    Ok(())
}

pub fn hosts_in_active_rollouts(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT h.agent_id, rh.rollout_id FROM rollout_hosts rh
         JOIN rollouts r ON r.rollout_id = rh.rollout_id
         JOIN hosts h ON h.host_id = rh.host_id
         WHERE r.state NOT IN {ROLLOUT_ENDED} AND r.state <> 'draft'
           AND rh.state NOT IN ('skipped', 'cancelled')"
    ))?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

pub fn active_offers(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT h.agent_id, rh.upgrade_id FROM rollout_hosts rh
         JOIN rollouts r ON r.rollout_id = rh.rollout_id
         JOIN hosts h ON h.host_id = rh.host_id
         WHERE r.state = 'running' AND rh.state IN ('offered', 'accepted', 'staged')",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

pub fn unacknowledged_rollback_failures(
    conn: &Connection,
) -> rusqlite::Result<Vec<RollbackFailure>> {
    let mut stmt = conn.prepare(
        "SELECT h.agent_id, h.hostname, rh.rollout_id, rh.detail FROM rollout_hosts rh
         JOIN hosts h ON h.host_id = rh.host_id
         WHERE rh.state = 'rollback_failed' AND rh.acknowledged_at IS NULL",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    rows.collect()
}

pub fn applying_hosts(conn: &Connection) -> rusqlite::Result<Vec<ApplyingHost>> {
    let mut stmt = conn.prepare(
        "SELECT h.agent_id, rh.rollout_id, COALESCE(rh.applying_at, rh.updated_at), r.options
         FROM rollout_hosts rh
         JOIN hosts h ON h.host_id = rh.host_id
         JOIN rollouts r ON r.rollout_id = rh.rollout_id
         WHERE rh.state = 'applying'",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    rows.collect()
}

pub fn rollouts_using_release(conn: &Connection, release_id: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM rollouts WHERE release_id = ?1 AND state NOT IN {ROLLOUT_ENDED}"
        ),
        [release_id],
        |r| r.get(0),
    )
}

/// 各推送、各主機狀態的台數（推送清單的進度；0.10.1）。只讀 `rollout_hosts`（不 JOIN `hosts`），
/// 一次彙總所有推送，回傳的列數是「推送數 × 出現過的狀態數」而不是主機數
pub fn rollout_host_counts(
    conn: &Connection,
    state: Option<&str>,
) -> rusqlite::Result<Vec<RolloutHostCount>> {
    let mut stmt = conn.prepare(
        "SELECT rh.rollout_id, rh.state, COUNT(*),
                SUM(CASE WHEN rh.state = 'rollback_failed' AND rh.acknowledged_at IS NULL THEN 1
                         WHEN rh.state = 'pending' AND rh.reason = 'artifact_missing' THEN 1
                         ELSE 0 END)
         FROM rollout_hosts rh
         WHERE ?1 IS NULL
            OR rh.rollout_id IN (SELECT r.rollout_id FROM rollouts r WHERE r.state = ?1)
         GROUP BY rh.rollout_id, rh.state
         ORDER BY rh.rollout_id, rh.state",
    )?;
    let rows = stmt.query_map([state], |r| {
        Ok(RolloutHostCount {
            rollout_id: r.get(0)?,
            state: r.get(1)?,
            hosts: r.get(2)?,
            flagged: r.get(3)?,
        })
    })?;
    rows.collect()
}

/// 各狀態的推送數（計數泡泡；0.10.1）
pub fn rollout_state_counts(conn: &Connection) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt =
        conn.prepare("SELECT state, COUNT(*) FROM rollouts GROUP BY state ORDER BY state")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// 回滾失敗、尚未標記為已處理的主機數：條件與 [`unacknowledged_rollback_failures`] 相同
pub fn count_unacknowledged_rollback_failures(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM rollout_hosts rh
         JOIN hosts h ON h.host_id = rh.host_id
         WHERE rh.state = 'rollback_failed' AND rh.acknowledged_at IS NULL",
        [],
        |r| r.get(0),
    )
}
