//! Pulse ↔ Kairos 協定（v1、v2）的共用實作（商業審計 A3、A4、A8、A14）。
//!
//! 規格只有一份：本 crate 目錄下的 [`PROTOCOL.md`](../PROTOCOL.md)。本 crate 的單元測試以
//! `include_str!` 讀入該文件，直接從「範例」節解析測試向量、從「錯誤碼」表比對 fatal 預設值，
//! 修改規格時不需要再到兩個產品裡各改一份複本。
//!
//! 內容：
//! - 協定常數（版本範圍、單行上限、欄位長度上限、心跳與離線門檻、指標上限）
//! - 訊息型別 [`ClientMsg`]、[`ServerMsg`] 與錯誤碼 [`ErrorCode`]，兩個方向都能序列化與反序列化
//!   （Kairos 解析 client 訊息、送出 server 訊息；Pulse 相反）
//! - 版本協商（A3）：[`client_proto_range`]、[`negotiate`]、[`Session`]
//! - at-least-once（A5）：durable 的 [`DurableSeq`]（`boot_id`／`report_seq`）與其限制
//! - 欄位驗證：UUID 格式的 `agent_id`、顯示用字串的禁止字元表、hello（含 v2 的主機資訊）、
//!   `ts`、report（v1 三欄位與 v2 的通用指標，A4）的限制
//! - 8443 的憑證正常輪替與除役通知（feature `renew`、`deregister`，PROTOCOL.md「憑證輪替與除役」）：
//!   [`lifecycle`] 模組與 [`ClientMsg`]／[`ServerMsg`] 的對應變體
//! - 註冊通道（8444 HTTPS，[`ENROLL.md`](../ENROLL.md)）：[`enroll`] 模組
//! - 指標擴充（商業審計 F4，[`METRICS.md`](../METRICS.md)）：F4 新增的標準指標與計數器速率
//!   （[`STANDARD_METRICS`]、[`counter_rate`]）、行程排行（feature `processes`，[`ProcessList`]）與
//!   主機資產（feature `inventory`，[`Inventory`]、[`ClientMsg::Inventory`]）
//! - 叢集連線（feature `cluster`，PROTOCOL.md「叢集與排空」）：`hello_ack.server_node`／`cluster_id`
//!   與排空訊息 [`ServerMsg::Drain`]
//! - 遠端管理（feature `config`、`upgrade`，PROTOCOL.md「遠端管理」、[`REMOTE.md`](../REMOTE.md)）：
//!   [`mgmt`] 模組（新訊息、hello 的 `install`、`pending`、驗證、協商、設定白名單 [`mgmt::settings`]）與
//!   [`release`] 模組（SSHSIG 簽章驗證、`allowed_signers`、`SHA256SUMS`、發佈檔名、發佈清單、版本比較）
//!
//! 本 crate 只有純函式與資料型別，不做 I/O、不記日誌，也不依賴 tokio；NDJSON 的分行 codec
//! 與日誌的措辭留在各產品。

#![forbid(unsafe_code)]

mod b64;
mod cluster;
pub mod enroll;
mod inventory;
pub mod lifecycle;
mod messages;
mod metrics;
pub mod mgmt;
mod negotiate;
mod processes;
pub mod release;
mod validate;

#[cfg(test)]
mod metrics_spec_tests;

#[cfg(test)]
mod tests;

pub use cluster::{
    DEFAULT_DRAIN_RECONNECT_MS, DRAIN_IN_FLIGHT_WAIT_MS, DRAIN_MS_PER_CONNECTION,
    DRAIN_REASON_NODE_DRAINING, DRAIN_REASON_REBALANCE, FEATURE_CLUSTER, MAX_DRAIN_RECONNECT_MS,
    MAX_SERVER_NODE_BYTES, MIN_DRAIN_RECONNECT_MS, drain_reconnect_within_ms,
    effective_drain_reconnect_ms, is_server_node, node_name_from_hostname,
};
pub use inventory::{
    CpuInventory, DISK_KINDS, DiskInventory, FEATURE_INVENTORY, INVENTORY_MIN_INTERVAL_MS,
    InterfaceInventory, Inventory, InventoryReport, MAX_INTERFACE_ADDRESSES, MAX_INVENTORY_DISKS,
    MAX_INVENTORY_INTERFACES, MAX_INVENTORY_TEXT_BYTES, MemoryInventory, OsInventory,
    SystemInventory, inventory_digest, is_ip_cidr, is_mac_address, validate_inventory,
};
pub use lifecycle::{
    Deregister, DeregisterAck, FEATURE_DEREGISTER, FEATURE_RENEW, INTENT_DEREGISTER,
    LIFECYCLE_FEATURES, RenewRequest, RenewResult,
};
pub use messages::{
    ClientMsg, DurableSeq, ErrorCode, HelloExt, HostInfo, LegacyReport, OsInfo, Proto, ProtoOffer,
    ReportBody, SERVER_MSG_TYPES, ServerMsg, ServerMsgError, hello_seq_of_invalid, message_type,
    parse_client_msg, parse_server_msg,
};
pub use metrics::{
    CounterSample, LEGACY_STANDARD_METRICS, METRIC_CPU_LOAD_1M, METRIC_CPU_LOAD_5M,
    METRIC_CPU_LOAD_15M, METRIC_CPU_USAGE, METRIC_DISK_IO_TIME, METRIC_DISK_READ,
    METRIC_DISK_READ_OPS, METRIC_DISK_WRITE_OPS, METRIC_DISK_WRITTEN, METRIC_FILESYSTEM_AVAILABLE,
    METRIC_FILESYSTEM_TOTAL, METRIC_FILESYSTEM_USAGE, METRIC_FILESYSTEM_USED,
    METRIC_MEMORY_AVAILABLE, METRIC_MEMORY_TOTAL, METRIC_MEMORY_USED,
    METRIC_NETWORK_ERRORS_RECEIVED, METRIC_NETWORK_ERRORS_TRANSMITTED,
    METRIC_NETWORK_PACKETS_RECEIVED, METRIC_NETWORK_PACKETS_TRANSMITTED, METRIC_NETWORK_RECEIVED,
    METRIC_NETWORK_TRANSMITTED, METRIC_PROCESS_COUNT, METRIC_SWAP_TOTAL, METRIC_SWAP_USED,
    METRIC_TEMPERATURE, METRIC_UPTIME, Metric, MetricKind, STANDARD_METRICS, StandardMetric,
    counter_rate, filesystem_usage, is_valid_label_key, is_valid_metric_name, is_valid_unit,
    standard_metric, validate_metric, validate_metrics,
};
pub use mgmt::{
    ConfigGet, ConfigStatus, FEATURE_CONFIG, FEATURE_UPGRADE, InstallInfo, MGMT_FEATURES,
    MgmtSession, Pending, RemoteConfig, UpgradeChunk, UpgradeChunkGet, UpgradeGet, UpgradeOffer,
    UpgradeStatus,
};
pub use negotiate::{
    FEATURE_DURABLE, FEATURE_METRICS, ProtoRange, ProtoRangeError, SERVER_FEATURES,
    SUPPORTED_PROTO, Session, client_proto_range, negotiate, unsupported_version_error,
};
pub use processes::{
    FEATURE_PROCESSES, MAX_PROCESS_COMMAND_BYTES, MAX_PROCESS_NAME_BYTES, MAX_TOP_PROCESSES,
    ProcessEntry, ProcessList, normalize_process_cpu, sanitize_process_command,
    validate_process_list,
};
pub use validate::{
    clamp_heartbeat_interval_ms, is_forbidden_display_char, is_host_fingerprint, is_uuid_text,
    normalize_agent_id, offline_after_ms, sanitize_display_text, server_cpu_usage,
    validate_client_msg, validate_durable_seq, validate_features, validate_hello,
    validate_host_info, validate_report,
};

/// 協定 v1：hello 只有 `proto`，report 為 `cpu_usage`／`total_memory`／`used_memory` 三欄位
pub const PROTO_V1: u32 = 1;

/// 協定 v2：hello 帶版本範圍、features 與主機資訊，hello_ack 帶 features 與 `server_time`，
/// report 可改用通用指標 `metrics`（雙方都宣告 [`FEATURE_METRICS`] 時）
pub const PROTO_V2: u32 = 2;

/// 單行上限（bytes，不含換行字元 `\n`，但含 `\r`）
pub const MAX_LINE_LENGTH: usize = 65536;

/// 同一條連線連續幾則無效訊息（`bad_request` 與 `unknown_type` 合計，中間沒有合法訊息）後，
/// 該則錯誤改為 fatal 並斷線
pub const MAX_CONSECUTIVE_BAD_REQUESTS: u32 = 5;

/// 離線判定的下限（毫秒）
pub const MIN_OFFLINE_AFTER_MS: u64 = 5000;

/// 心跳間隔的倍數，超過此時間沒收到訊息即視為離線
pub const OFFLINE_HEARTBEAT_MULTIPLIER: u64 = 5;

/// 伺服器接受的心跳間隔範圍（毫秒），超出範圍會被夾值。
/// Pulse 的 `heartbeat.interval` 設定上限（60 秒）與 [`MAX_HEARTBEAT_INTERVAL_MS`] 一致。
pub const MIN_HEARTBEAT_INTERVAL_MS: u64 = 1000;
pub const MAX_HEARTBEAT_INTERVAL_MS: u64 = 60000;

/// hello 欄位長度上限（bytes）
pub const MAX_HOSTNAME_BYTES: usize = 255;
pub const MAX_AGENT_VERSION_BYTES: usize = 64;

/// `ts` 欄位長度上限（bytes）
pub const MAX_TS_BYTES: usize = 64;

/// v2 hello：`features` 的項目數與每項長度上限（client 與 server 相同）
pub const MAX_FEATURES: usize = 32;
pub const MAX_FEATURE_BYTES: usize = 64;

/// v2 hello：`display_name` 長度上限（bytes，同 `hostname`）
pub const MAX_DISPLAY_NAME_BYTES: usize = 255;

/// v2 hello：`tags` 的項目數與每項長度上限（bytes）
pub const MAX_TAGS: usize = 32;
pub const MAX_TAG_BYTES: usize = 64;

/// v2 hello：`os` 物件中每個字串欄位的長度上限（bytes）
pub const MAX_OS_FIELD_BYTES: usize = 128;

/// v2 hello：`host_fingerprint` 長度上限（bytes）；格式見 [`is_host_fingerprint`]
pub const MAX_HOST_FINGERPRINT_BYTES: usize = 145;

/// v2 hello：`report_interval_ms` 的接受範圍（毫秒，1 秒～24 小時；超出時 `bad_request`）
pub const MIN_REPORT_INTERVAL_MS: u64 = 1000;
pub const MAX_REPORT_INTERVAL_MS: u64 = 86_400_000;

/// v2 report：每筆 report 的指標數上限
pub const MAX_METRICS: usize = 256;
/// 指標名稱長度上限（bytes）
pub const MAX_METRIC_NAME_BYTES: usize = 128;
/// 指標單位長度上限（bytes）
pub const MAX_METRIC_UNIT_BYTES: usize = 32;
/// 每個指標的 label 數上限
pub const MAX_METRIC_LABELS: usize = 16;
/// label 名稱長度上限（bytes）
pub const MAX_LABEL_KEY_BYTES: usize = 64;
/// label 值長度上限（bytes）
pub const MAX_LABEL_VALUE_BYTES: usize = 256;

/// durable：伺服器至少保證去重的重送範圍。同一個 (agent_id, `boot_id`) 中，`report_seq` 不小於
/// 「已保存的最大 `report_seq` − 此值 + 1」的重送都能正確判斷是否已保存；伺服器可以把更舊的重送
/// 一律視為已保存（回 `ack`、不寫入；Kairos 的 JSONL 後端如此，SQLite 後端以唯一索引判斷整段保存
/// 期間）。client 依序送出、在途筆數不超過 [`MAX_DURABLE_IN_FLIGHT`] 時，需要重送的資料一定落在
/// 這個範圍內
pub const DURABLE_DEDUP_WINDOW: u64 = 256;

/// durable：client 同一時間最多在途（已送出、尚未收到回應）的 report 筆數
pub const MAX_DURABLE_IN_FLIGHT: usize = 8;

const _: () = assert!(MAX_DURABLE_IN_FLIGHT as u64 <= DURABLE_DEDUP_WINDOW);
