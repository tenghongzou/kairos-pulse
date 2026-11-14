//! 日誌行的格式（設計包 `pulse/log-ux.md` §6；兩端共用，`log-ux.md` §8「與 Kairos 的對齊」）。
//!
//! | `log.format` | 一行的樣子 |
//! |---|---|
//! | `text`（預設） | `2026-10-14T14:22:16.123+08:00 [WARN] buffer.full PLS-W4202 本機緩衝已滿…` |
//! | `json` | `{"v":1,"ts":"2026-10-14T06:22:16.123Z","level":"warn","event":"buffer.full","code":"PLS-W4202","msg":"本機緩衝已滿…",…}` |
//! | `legacy` | `2026-10-14 14:22:16 [WARN] buffer.full PLS-W4202 本機緩衝已滿…`（0.5 以前的格式，本地時間不帶時區） |
//!
//! `text` 的時間為 RFC 3339、毫秒、帶時區位移（`log.timezone = "utc"` 時為 `Z`）；其餘部分與 0.5
//! 相同（`[等級] 訊息`），既有的 grep 與解析規則只需調整時間的部分。`legacy` 保留一個主要版本供過渡。
//!
//! ## 事件與代碼
//!
//! JSON 的 `event`、`code` 依序取自：
//! 1. 日誌的結構化欄位（`log` 的 kv）`event`、`code`：`info!(event = "agent.start"; "…")`；
//! 2. 訊息開頭的「`<event> <code> `」（Pulse 既有的寫法，`code` 為 `-` 表示沒有）；
//! 3. `code` 另外會從訊息中找第一個 `KRS-`／`PLS-` 代碼（Kairos 既有的「警告 KRS-W3005：…」寫法）；
//!    `event` 沒有時以模組路徑代替（例如 `kairos.enroll.handlers`），標示「尚未指定固定事件」。
//!
//! 其餘的結構化欄位放在 JSON 的 `fields` 物件中（不攤平到頂層，避免與固定欄位撞名；SIEM 以
//! `fields.*` 對應）；`text` 時附在訊息後，兩個空白分隔，`key=value`。

use chrono::{DateTime, Local, Utc};
use log::kv::{Key, Value, VisitSource};
use log::{Level, Record};
use std::collections::hash_map::RandomState;
use std::fmt::Write as _;
use std::hash::{BuildHasher, Hasher};
use std::io::Write as _;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// JSON 日誌的 schema 版本（`log-ux.md` §6.2 的 `v`）
pub const JSON_SCHEMA_VERSION: u32 = 1;

/// `log.format`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    #[default]
    Text,
    Json,
    Legacy,
}

impl LogFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "text" => Some(LogFormat::Text),
            "json" => Some(LogFormat::Json),
            "legacy" => Some(LogFormat::Legacy),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LogFormat::Text => "text",
            LogFormat::Json => "json",
            LogFormat::Legacy => "legacy",
        }
    }

    /// 日誌檔的副檔名：JSON 為 `.jsonl`（兩種格式不混在同一個檔案，`log-ux.md` §7）
    pub fn file_suffix(self) -> &'static str {
        match self {
            LogFormat::Json => ".jsonl",
            _ => ".log",
        }
    }
}

/// `log.timezone`（只影響 `text`；`json` 一律 UTC，`legacy` 一律本地時間）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogTimezone {
    #[default]
    Local,
    Utc,
}

impl LogTimezone {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "local" => Some(LogTimezone::Local),
            "utc" => Some(LogTimezone::Utc),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LogTimezone::Local => "local",
            LogTimezone::Utc => "utc",
        }
    }
}

/// 每一行共用的資訊：產品、主機名稱、本次行程的 `run_id`、遞增的 `seq`，以及 Pulse 的 agent_id
#[derive(Debug)]
pub struct LogMeta {
    pub product: &'static str,
    pub host: String,
    pub run_id: String,
    /// Pulse：JSON 每行都有 `agent_id` 與 `instance`（尚未取得時為 `null`）；Kairos 不輸出這兩個欄位
    pub agent_fields: bool,
    seq: AtomicU64,
    agent_id: RwLock<Option<String>>,
    /// Pulse 的多實例編號（`pulse-N.service`；單一實例時為 `None`，JSON 為 `null`）
    instance: RwLock<Option<u32>>,
    /// Kairos 的叢集節點名稱（只在叢集模式設定；設定後 JSON 每行多一個 `node_id`，否則不輸出這個欄位）
    node_id: RwLock<Option<String>>,
}

impl LogMeta {
    pub fn new(product: &'static str, host: String, agent_fields: bool) -> Self {
        Self {
            product,
            host,
            run_id: new_run_id(),
            agent_fields,
            seq: AtomicU64::new(0),
            agent_id: RwLock::new(None),
            instance: RwLock::new(None),
            node_id: RwLock::new(None),
        }
    }

    /// 設定 JSON 的 `node_id`（Kairos 的叢集模式；`kairos/HA.md` 第 14.5 節）
    pub fn set_node_id(&self, id: Option<String>) {
        *self.node_id.write().unwrap_or_else(|e| e.into_inner()) = id;
    }

    pub fn node_id(&self) -> Option<String> {
        self.node_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 設定 JSON 的 `instance`（Pulse 的多實例 N；設計包 `log-ux.md` §6.2）
    pub fn set_instance(&self, n: Option<u32>) {
        *self.instance.write().unwrap_or_else(|e| e.into_inner()) = n;
    }

    pub fn instance(&self) -> Option<u32> {
        *self.instance.read().unwrap_or_else(|e| e.into_inner())
    }

    /// 設定 JSON 的 `agent_id`（Pulse 讀取身分之後；未啟用 mTLS 時不設定，agent_id 等同身分憑據）
    pub fn set_agent_id(&self, id: Option<String>) {
        *self.agent_id.write().unwrap_or_else(|e| e.into_inner()) = id;
    }

    pub fn agent_id(&self) -> Option<String> {
        self.agent_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// 本機名稱（日誌的 `host` 與 syslog 的 HOSTNAME）：Linux 讀 `/proc/sys/kernel/hostname`，
/// 其他平台用 `COMPUTERNAME`／`HOSTNAME` 環境變數；都沒有時為 `localhost`
pub fn hostname() -> String {
    let from_proc = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string());
    from_proc
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

/// 本次行程的隨機 ID（`r-` 加 12 個十六進位字元）：以標準函式庫每個行程隨機的雜湊金鑰產生，
/// 不需要額外的亂數套件；只用來區分重新啟動前後的事件，不是秘密
pub fn new_run_id() -> String {
    let mut h = RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    h.write_u32(std::process::id());
    format!("r-{:012x}", h.finish() & 0xffff_ffff_ffff)
}

/// 每個等級的行數（`/metrics` 的 `*_log_messages_total`）：error、warn、info、debug、trace
static LEVEL_COUNTS: [AtomicU64; 5] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

fn level_index(level: Level) -> usize {
    match level {
        Level::Error => 0,
        Level::Warn => 1,
        Level::Info => 2,
        Level::Debug => 3,
        Level::Trace => 4,
    }
}

/// 記錄一行（由產品的 logger 在每一行呼叫一次）
pub fn count_line(level: Level) {
    LEVEL_COUNTS[level_index(level)].fetch_add(1, Ordering::Relaxed);
}

/// 啟動以來各等級的行數：`[("error", n), ("warn", n), …]`
pub fn line_counts() -> [(&'static str, u64); 5] {
    let get = |i: usize| LEVEL_COUNTS[i].load(Ordering::Relaxed);
    [
        ("error", get(0)),
        ("warn", get(1)),
        ("info", get(2)),
        ("debug", get(3)),
        ("trace", get(4)),
    ]
}

/// 是否為錯誤代碼：`KRS-`／`PLS-` 加上 `E`／`W`／`I` 與 4 位數字
pub fn is_code(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 9
        && (s.starts_with("KRS-") || s.starts_with("PLS-"))
        && matches!(b[4], b'E' | b'W' | b'I')
        && b[5..].iter().all(u8::is_ascii_digit)
}

/// 訊息中第一個錯誤代碼
pub fn find_code(msg: &str) -> Option<&str> {
    let mut from = 0;
    while let Some(pos) = msg[from..]
        .find("KRS-")
        .into_iter()
        .chain(msg[from..].find("PLS-"))
        .min()
    {
        let start = from + pos;
        if let Some(candidate) = msg.get(start..start + 9)
            && is_code(candidate)
            && !msg[start + 9..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
        {
            return Some(candidate);
        }
        from = start + 4;
    }
    None
}

/// 是否為事件名稱：小寫，以點分隔的兩段以上（`conn.lost`、`cert.grace_renew_blocked`）
pub fn is_event_name(s: &str) -> bool {
    let mut parts = 0;
    for part in s.split('.') {
        let mut chars = part.chars();
        match chars.next() {
            Some(c) if c.is_ascii_lowercase() => {}
            _ => return false,
        }
        if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return false;
        }
        parts += 1;
    }
    parts >= 2
}

/// 訊息開頭的「`<event> <code> `」（Pulse 的寫法）；回傳 (event, code, 其餘的訊息)
pub fn split_prefix(msg: &str) -> Option<(&str, Option<&str>, &str)> {
    let (event, rest) = msg.split_once(' ')?;
    if !is_event_name(event) {
        return None;
    }
    let (code, rest) = rest.split_once(' ').unwrap_or((rest, ""));
    if code == "-" {
        Some((event, None, rest))
    } else if is_code(code) {
        Some((event, Some(code), rest))
    } else {
        None
    }
}

/// 模組路徑代替的事件名稱：`kairos::enroll::handlers` → `kairos.enroll.handlers`
pub fn fallback_event(target: &str) -> String {
    let t = target.replace("::", ".").to_ascii_lowercase();
    if t.is_empty() { "log".to_string() } else { t }
}

/// 結構化欄位（kv）轉成的 JSON 值
fn kv_json(v: &Value<'_>) -> serde_json::Value {
    if let Some(b) = v.to_bool() {
        serde_json::Value::Bool(b)
    } else if let Some(n) = v.to_u64() {
        serde_json::Value::from(n)
    } else if let Some(n) = v.to_i64() {
        serde_json::Value::from(n)
    } else if let Some(f) = v.to_f64() {
        serde_json::Number::from_f64(f).map_or(serde_json::Value::Null, serde_json::Value::Number)
    } else if let Some(s) = v.to_borrowed_str() {
        serde_json::Value::String(s.to_string())
    } else {
        serde_json::Value::String(v.to_string())
    }
}

#[derive(Default)]
struct Kv {
    event: Option<String>,
    code: Option<String>,
    fields: Vec<(String, serde_json::Value)>,
}

impl<'kvs> VisitSource<'kvs> for Kv {
    fn visit_pair(&mut self, key: Key<'kvs>, value: Value<'kvs>) -> Result<(), log::kv::Error> {
        match key.as_str() {
            "event" => self.event = Some(value.to_string()),
            "code" => self.code = Some(value.to_string()).filter(|c| c != "-" && !c.is_empty()),
            k => self.fields.push((k.to_string(), kv_json(&value))),
        }
        Ok(())
    }
}

/// 一行日誌的組成（格式化前）
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub level: Level,
    pub target: String,
    /// 固定事件名稱；`None` 表示沒有指定（JSON 以模組路徑代替）
    pub event: Option<String>,
    pub code: Option<String>,
    /// 完整的訊息（`text`、`legacy` 原樣輸出）
    pub message: String,
    /// 去掉開頭「`<event> <code> `」的訊息（JSON 的 `msg`）
    pub msg: String,
    pub fields: Vec<(String, serde_json::Value)>,
}

impl Line {
    /// 由 `log` 的紀錄組成（`message` 為已格式化的訊息）
    pub fn from_record(record: &Record<'_>, message: String) -> Self {
        let mut kv = Kv::default();
        let _ = record.key_values().visit(&mut kv);
        Self::new(record.level(), record.target(), message, kv)
    }

    fn new(level: Level, target: &str, message: String, kv: Kv) -> Self {
        let (mut event, mut code, msg) = match split_prefix(&message) {
            Some((e, c, rest)) => (Some(e.to_string()), c.map(str::to_string), rest.to_string()),
            None => (None, None, message.clone()),
        };
        if kv.event.is_some() {
            event = kv.event;
        }
        if kv.code.is_some() {
            code = kv.code;
        }
        if code.is_none() {
            code = find_code(&msg).map(str::to_string);
        }
        Self {
            level,
            target: target.to_string(),
            event,
            code,
            message,
            msg,
            fields: kv.fields,
        }
    }

    /// 測試與工具用：只有等級、模組與訊息
    pub fn plain(level: Level, target: &str, message: &str) -> Self {
        Self::new(level, target, message.to_string(), Kv::default())
    }

    /// 帶結構化欄位（測試用）
    pub fn with_fields(mut self, fields: Vec<(String, serde_json::Value)>) -> Self {
        self.fields = fields;
        self
    }

    /// JSON 的 `event`（沒有指定時為模組路徑）
    pub fn event_name(&self) -> String {
        self.event
            .clone()
            .unwrap_or_else(|| fallback_event(&self.target))
    }
}

/// RFC 3339、毫秒：本地時間帶時區位移，或 UTC 以 `Z` 結尾
pub fn rfc3339(now: DateTime<Utc>, tz: LogTimezone) -> String {
    match tz {
        LogTimezone::Utc => now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        LogTimezone::Local => now
            .with_timezone(&Local)
            .format("%Y-%m-%dT%H:%M:%S%.3f%:z")
            .to_string(),
    }
}

fn text_field(out: &mut String, key: &str, value: &serde_json::Value) {
    let v = match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if v.is_empty() || v.contains([' ', '"', '\t', '\n', '\r', '=']) {
        let _ = write!(out, " {key}={}", serde_json::Value::String(v));
    } else {
        let _ = write!(out, " {key}={v}");
    }
}

/// 一行（不含換行）
pub fn format_line(
    line: &Line,
    meta: &LogMeta,
    format: LogFormat,
    tz: LogTimezone,
    now: DateTime<Utc>,
) -> String {
    match format {
        LogFormat::Json => format_json(line, meta, now),
        LogFormat::Text | LogFormat::Legacy => {
            let ts = if format == LogFormat::Legacy {
                now.with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            } else {
                rfc3339(now, tz)
            };
            format!("{ts} [{}] {}", line.level, format_body(line))
        }
    }
}

/// `text` 的時間與等級之後的部分（syslog 的 `payload = "text"` 也使用）
pub fn format_body(line: &Line) -> String {
    let mut out = String::with_capacity(line.message.len() + 32);
    // 由 kv 指定事件、但訊息沒有寫在開頭時補上，與 Pulse 既有的「<event> <code> 訊息」一致
    if let Some(event) = &line.event
        && split_prefix(&line.message).is_none()
    {
        let _ = write!(out, "{event} {} ", line.code.as_deref().unwrap_or("-"));
    }
    // 訊息本身可能帶外部來源的內容（例如 SMTP／OTLP 伺服器的回應被回顯進記錄的訊息）：
    // 跳脫控制字元，避免 CR／LF 在文字格式的記錄中偽造出額外的日誌行（E6）。欄位值由
    // text_field 以 JSON 編碼，已經安全
    push_escaped(&mut out, &line.message);
    if !line.fields.is_empty() {
        out.push(' ');
        for (k, v) in &line.fields {
            text_field(&mut out, k, v);
        }
    }
    out
}

/// 把 `s` 附加到 `out`，以 `\n`／`\r`／`\t` 與 `\xNN` 跳脫控制字元（C0 與 DEL），
/// 其餘字元原樣輸出。用於文字格式的記錄，防止換行注入偽造日誌行
fn push_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c.is_control()) => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// syslog 的 MSGID：錯誤代碼，沒有時為事件名稱（沒有指定事件時為 `-`）
pub fn msgid(line: &Line) -> &str {
    line.code
        .as_deref()
        .or(line.event.as_deref())
        .unwrap_or("-")
}

/// 一個日誌輸出：每一行格式化一次，寫到檔案、（可選）stdout 與 syslog。
/// 檔案的寫入在鎖內進行（一行一次 `write_all` 再 `flush`），多執行緒同時記錄時行不會交錯
pub struct LogSink {
    pub meta: std::sync::Arc<LogMeta>,
    pub format: LogFormat,
    pub tz: LogTimezone,
    file: std::sync::Mutex<Box<dyn std::io::Write + Send>>,
    stdout: bool,
    pub syslog: Option<crate::syslog::Sender>,
}

impl LogSink {
    pub fn new(
        meta: std::sync::Arc<LogMeta>,
        format: LogFormat,
        tz: LogTimezone,
        file: Box<dyn std::io::Write + Send>,
        stdout: bool,
        syslog: Option<crate::syslog::Sender>,
    ) -> Self {
        Self {
            meta,
            format,
            tz,
            file: std::sync::Mutex::new(file),
            stdout,
            syslog,
        }
    }

    /// 記錄一行（`fern::Output::call` 或自訂的 `log::Log` 呼叫）
    pub fn log(&self, record: &Record<'_>) {
        count_line(record.level());
        let line = Line::from_record(record, record.args().to_string());
        let now = Utc::now();
        let text = format_line(&line, &self.meta, self.format, self.tz, now);
        let mut buf = String::with_capacity(text.len() + 1);
        buf.push_str(&text);
        buf.push('\n');
        {
            let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());
            let _ = f.write_all(buf.as_bytes());
            let _ = f.flush();
        }
        if self.stdout {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(buf.as_bytes());
            let _ = out.flush();
        }
        if let Some(s) = &self.syslog {
            let msg = match s.config.payload {
                crate::syslog::Payload::Json if self.format == LogFormat::Json => text,
                crate::syslog::Payload::Json => format_json(&line, &self.meta, now),
                crate::syslog::Payload::Text => format_body(&line),
            };
            s.log(line.level, now, msgid(&line), &msg);
        }
    }

    pub fn flush(&self) {
        let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());
        let _ = f.flush();
    }
}

/// 依 UTC 日期分檔的日誌檔（`<dir>/<prefix>YYYY-MM-DD<suffix>`）：換日時關閉舊檔、開啟新日期的檔案。
/// 只在一行的第一次 `write` 檢查日期（[`LogSink`] 每行寫一次再 `flush`），同一行不會被拆到兩個檔案
pub struct DailyFile {
    dir: std::path::PathBuf,
    prefix: &'static str,
    suffix: &'static str,
    open: fn(&std::path::Path) -> std::io::Result<std::fs::File>,
    clock: fn() -> DateTime<Utc>,
    current: Option<(chrono::NaiveDate, std::io::BufWriter<std::fs::File>)>,
    record_start: bool,
}

impl DailyFile {
    pub fn new(
        dir: std::path::PathBuf,
        prefix: &'static str,
        suffix: &'static str,
        open: fn(&std::path::Path) -> std::io::Result<std::fs::File>,
        clock: fn() -> DateTime<Utc>,
    ) -> Self {
        Self {
            dir,
            prefix,
            suffix,
            open,
            clock,
            current: None,
            record_start: true,
        }
    }

    /// 某日期的檔名
    pub fn file_name(&self, date: chrono::NaiveDate) -> String {
        format!("{}{}{}", self.prefix, date.format("%Y-%m-%d"), self.suffix)
    }

    fn file(&mut self) -> std::io::Result<&mut std::io::BufWriter<std::fs::File>> {
        if self.record_start || self.current.is_none() {
            let today = (self.clock)().date_naive();
            if self.current.as_ref().map(|(d, _)| *d) != Some(today) {
                if let Some((_, mut old)) = self.current.take() {
                    let _ = old.flush();
                }
                let file = (self.open)(&self.dir.join(self.file_name(today)))?;
                self.current = Some((today, std::io::BufWriter::new(file)));
            }
            self.record_start = false;
        }
        self.current
            .as_mut()
            .map(|(_, f)| f)
            .ok_or_else(|| std::io::Error::other("日誌檔未開啟"))
    }
}

impl std::io::Write for DailyFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file()?.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.record_start = true;
        match self.current.as_mut() {
            Some((_, f)) => f.flush(),
            None => Ok(()),
        }
    }
}

fn json_str(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// JSON 一行（`log-ux.md` §6.2；欄位順序固定）
pub fn format_json(line: &Line, meta: &LogMeta, now: DateTime<Utc>) -> String {
    let mut out = String::with_capacity(256 + line.message.len());
    let _ = write!(
        out,
        "{{\"v\":{JSON_SCHEMA_VERSION},\"ts\":{},\"level\":\"{}\",\"event\":{},\"code\":{},\"msg\":{}",
        json_str(&rfc3339(now, LogTimezone::Utc)),
        line.level.as_str().to_ascii_lowercase(),
        json_str(&line.event_name()),
        line.code.as_deref().map_or("null".to_string(), json_str),
        json_str(&line.msg),
    );
    if meta.agent_fields {
        let id = meta.agent_id();
        let _ = write!(
            out,
            ",\"agent_id\":{},\"instance\":{}",
            id.as_deref().map_or("null".to_string(), json_str),
            meta.instance()
                .map_or("null".to_string(), |n| n.to_string())
        );
    }
    let _ = write!(
        out,
        ",\"product\":{},\"host\":{}",
        json_str(meta.product),
        json_str(&meta.host),
    );
    // 叢集節點名稱：只在設定了的時候輸出（單機的輸出不變）
    if let Some(node) = meta.node_id() {
        let _ = write!(out, ",\"node_id\":{}", json_str(&node));
    }
    let _ = write!(
        out,
        ",\"run_id\":{},\"seq\":{},\"target\":{},\"fields\":{{",
        json_str(&meta.run_id),
        meta.next_seq(),
        json_str(&line.target),
    );
    for (i, (k, v)) in line.fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}:{v}", json_str(k));
    }
    out.push_str("}}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn meta(agent: bool) -> LogMeta {
        LogMeta::new("pulse", "plc-hmi-07".into(), agent)
    }

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 9, 6, 22, 16).unwrap() + chrono::Duration::milliseconds(123)
    }

    #[test]
    fn codes_and_events_are_recognized() {
        assert!(is_code("KRS-W3005") && is_code("PLS-E2003") && is_code("KRS-I2010"));
        assert!(!is_code("KRS-X3005") && !is_code("ABC-E2003") && !is_code("PLS-E200"));
        assert_eq!(find_code("警告 KRS-W3005：已暫時封鎖"), Some("KRS-W3005"));
        assert_eq!(find_code("KRS-W30051 不是"), None);
        assert_eq!(find_code("沒有"), None);
        assert_eq!(find_code("PLS-E1 與 PLS-E2003"), Some("PLS-E2003"));
        assert!(is_event_name("conn.lost") && is_event_name("cert.grace_renew_blocked"));
        assert!(!is_event_name("conn") && !is_event_name("Conn.lost") && !is_event_name("a..b"));
        assert_eq!(
            split_prefix("buffer.full PLS-W4202 本機緩衝已滿"),
            Some(("buffer.full", Some("PLS-W4202"), "本機緩衝已滿"))
        );
        assert_eq!(
            split_prefix("agent.stop - Pulse 已結束"),
            Some(("agent.stop", None, "Pulse 已結束"))
        );
        assert_eq!(split_prefix("已連線 to x"), None);
        assert_eq!(split_prefix("a.b c"), None);
        assert_eq!(
            fallback_event("kairos::enroll::handlers"),
            "kairos.enroll.handlers"
        );
    }

    #[test]
    fn format_body_escapes_control_characters_in_the_message() {
        // 外部來源（SMTP／OTLP 回應）被回顯進訊息時，CR／LF 不得在文字格式中偽造日誌行（E6）
        let line = Line::plain(
            Level::Warn,
            "kairos::alerts",
            "SMTP 回應 250 ok\r\n2026-01-01T00:00:00Z [ERROR] 偽造的一行\ttab\x07bell",
        );
        let body = format_body(&line);
        assert!(
            !body.contains('\n') && !body.contains('\r') && !body.contains('\t'),
            "body 不得含原始控制字元：{body:?}"
        );
        assert!(body.contains("\\r\\n"), "{body}");
        assert!(body.contains("\\ttab"), "{body}");
        assert!(body.contains("\\x07bell"), "{body}");
        // 整個 body 仍是單一行
        assert_eq!(body.lines().count(), 1, "{body:?}");
    }

    #[test]
    fn json_line_has_fixed_fields_in_order() {
        let m = meta(true);
        m.set_agent_id(Some("9422cb70-1b2c-4d3e-9f00-112233445566".into()));
        let line = Line::plain(
            Level::Warn,
            "pulse::spool",
            "buffer.full PLS-W4202 本機緩衝已滿，丟掉最舊的資料",
        )
        .with_fields(vec![("dropped".into(), 3.into())]);
        let s = format_json(&line, &m, at());
        assert!(s.starts_with("{\"v\":1,\"ts\":\"2026-10-09T06:22:16.123Z\",\"level\":\"warn\",\"event\":\"buffer.full\",\"code\":\"PLS-W4202\",\"msg\":\"本機緩衝已滿，丟掉最舊的資料\""), "{s}");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["agent_id"], "9422cb70-1b2c-4d3e-9f00-112233445566");
        assert_eq!(v["instance"], serde_json::Value::Null);
        // 多實例：instance 為編號
        let multi = meta(true);
        multi.set_instance(Some(3));
        let v3: serde_json::Value =
            serde_json::from_str(&format_json(&line, &multi, at())).unwrap();
        assert_eq!(v3["instance"], 3);
        assert_eq!(v["host"], "plc-hmi-07");
        assert_eq!(v["product"], "pulse");
        assert_eq!(v["seq"], 1);
        assert_eq!(v["target"], "pulse::spool");
        assert_eq!(v["fields"]["dropped"], 3);
        assert!(v["run_id"].as_str().unwrap().starts_with("r-"));
        // seq 遞增
        let s2 = format_json(&line, &m, at());
        let v2: serde_json::Value = serde_json::from_str(&s2).unwrap();
        assert_eq!(v2["seq"], 2);
    }

    #[test]
    fn json_without_event_uses_module_and_finds_code() {
        let m = meta(false);
        let line = Line::plain(
            Level::Error,
            "kairos::enroll::pki_mgr",
            "錯誤 KRS-E1205：無法寫入 CRL \"x\"\n第二行",
        );
        let s = format_json(&line, &m, at());
        assert!(!s.contains('\n'), "一行一個物件：{s}");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["event"], "kairos.enroll.pki_mgr");
        assert_eq!(v["code"], "KRS-E1205");
        assert_eq!(v["level"], "error");
        assert_eq!(v["msg"], "錯誤 KRS-E1205：無法寫入 CRL \"x\"\n第二行");
        assert!(v.get("agent_id").is_none());
        assert_eq!(v["fields"], serde_json::json!({}));
        let line = Line::plain(Level::Info, "kairos", "沒有代碼");
        let v: serde_json::Value = serde_json::from_str(&format_json(&line, &m, at())).unwrap();
        assert_eq!(v["code"], serde_json::Value::Null);
    }

    #[test]
    fn text_has_rfc3339_with_offset_and_legacy_keeps_old_layout() {
        let m = meta(false);
        let line = Line::plain(Level::Info, "kairos", "Kairos 已啟動");
        let utc = format_line(&line, &m, LogFormat::Text, LogTimezone::Utc, at());
        assert_eq!(utc, "2026-10-09T06:22:16.123Z [INFO] Kairos 已啟動");
        let local = format_line(&line, &m, LogFormat::Text, LogTimezone::Local, at());
        let ts = local.split(' ').next().unwrap();
        let parsed = DateTime::parse_from_rfc3339(ts).unwrap();
        assert_eq!(parsed.with_timezone(&Utc), at());
        assert!(ts.len() == "2026-10-09T14:22:16.123+08:00".len(), "{ts}");
        assert!(local.ends_with(" [INFO] Kairos 已啟動"));
        let legacy = format_line(&line, &m, LogFormat::Legacy, LogTimezone::Utc, at());
        assert!(legacy.ends_with(" [INFO] Kairos 已啟動"));
        let ts = &legacy[..19];
        assert!(chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S").is_ok());
    }

    #[test]
    fn text_shows_kv_event_and_fields() {
        let m = meta(false);
        let mut kv = Kv {
            event: Some("agent.start".into()),
            ..Kv::default()
        };
        kv.fields.push(("pid".into(), 42.into()));
        kv.fields.push(("path".into(), "/opt/有 空白".into()));
        let line = Line::new(Level::Info, "kairos", "啟動".into(), kv);
        let s = format_line(&line, &m, LogFormat::Text, LogTimezone::Utc, at());
        assert_eq!(
            s,
            "2026-10-09T06:22:16.123Z [INFO] agent.start - 啟動  pid=42 path=\"/opt/有 空白\""
        );
    }

    #[test]
    fn kv_from_log_record() {
        let record = Record::builder()
            .level(Level::Warn)
            .target("kairos::x")
            .key_values(&[
                ("event", "conn.lost"),
                ("code", "KRS-W2201"),
                ("peer", "10.0.0.1"),
            ])
            .build();
        let line = Line::from_record(&record, "被拒".into());
        assert_eq!(line.event.as_deref(), Some("conn.lost"));
        assert_eq!(line.code.as_deref(), Some("KRS-W2201"));
        assert_eq!(
            line.fields,
            vec![("peer".to_string(), serde_json::json!("10.0.0.1"))]
        );
    }

    /// 測試用：可共用讀取的寫入目標
    #[derive(Clone, Default)]
    struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn sink_writes_one_line_per_record_and_forwards_to_syslog() {
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let sender = crate::syslog::Sender::start(crate::syslog::Config {
            transport: crate::syslog::Transport::Udp(rx.local_addr().unwrap().to_string()),
            facility: 3,
            app_name: "kairos".into(),
            hostname: "h".into(),
            payload: crate::syslog::Payload::Json,
        })
        .unwrap();
        let buf = Shared::default();
        let meta = std::sync::Arc::new(LogMeta::new("kairos", "h".into(), false));
        let sink = LogSink::new(
            meta,
            LogFormat::Json,
            LogTimezone::Utc,
            Box::new(buf.clone()),
            false,
            Some(sender.clone()),
        );
        sink.log(
            &Record::builder()
                .level(Level::Warn)
                .target("kairos")
                .args(format_args!("警告 KRS-W2201：來自 10.0.0.1 的連線已達上限"))
                .build(),
        );
        assert!(sender.flush(std::time::Duration::from_secs(5)));
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(text.ends_with('\n') && text.lines().count() == 1, "{text}");
        let v: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(v["code"], "KRS-W2201");
        let mut b = [0u8; 4096];
        let n = rx.recv(&mut b).unwrap();
        let got = std::str::from_utf8(&b[..n]).unwrap();
        assert!(got.starts_with("<28>1 "), "{got}");
        assert!(got.contains(" KRS-W2201 - {\"v\":1,"), "{got}");
        // syslog 與檔案是同一行（seq 相同）
        assert!(got.ends_with(text.trim_end()), "{got}");
    }

    static FILE_CLOCK: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

    fn file_clock() -> DateTime<Utc> {
        Utc.timestamp_opt(FILE_CLOCK.load(std::sync::atomic::Ordering::SeqCst), 0)
            .unwrap()
    }

    #[test]
    fn daily_file_switches_between_lines() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("kp-obs-daily-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d1 = Utc.with_ymd_and_hms(2026, 10, 9, 23, 59, 59).unwrap();
        FILE_CLOCK.store(d1.timestamp(), std::sync::atomic::Ordering::SeqCst);
        fn open(p: &std::path::Path) -> std::io::Result<std::fs::File> {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
        }
        let mut f = DailyFile::new(dir.clone(), "pulse-", ".jsonl", open, file_clock);
        f.write_all(b"a").unwrap();
        FILE_CLOCK.store(d1.timestamp() + 2, std::sync::atomic::Ordering::SeqCst);
        f.write_all(b"b\n").unwrap();
        f.flush().unwrap();
        f.write_all(b"c\n").unwrap();
        f.flush().unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("pulse-2026-10-09.jsonl")).unwrap(),
            "ab\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("pulse-2026-10-10.jsonl")).unwrap(),
            "c\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn options_parse() {
        assert_eq!(LogFormat::parse(" JSON "), Some(LogFormat::Json));
        assert_eq!(LogFormat::parse("xml"), None);
        assert_eq!(LogFormat::Json.file_suffix(), ".jsonl");
        assert_eq!(LogFormat::Legacy.file_suffix(), ".log");
        assert_eq!(LogTimezone::parse("UTC"), Some(LogTimezone::Utc));
        assert_eq!(LogTimezone::parse("tokyo"), None);
        let before = line_counts()[1].1;
        count_line(Level::Warn);
        assert!(line_counts()[1].1 > before);
        assert_ne!(new_run_id(), "");
    }
}
