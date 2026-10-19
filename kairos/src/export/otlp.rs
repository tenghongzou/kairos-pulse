//! OTLP/HTTP（protobuf）的推送（`kairos/EXPORT.md` 第 3 節）。
//!
//! - **傳輸**：`POST <export.otlp.endpoint>`，`Content-Type: application/x-protobuf`，本文為
//!   `ExportMetricsServiceRequest`（[`encode`]）。HTTP/1.1、一個請求一條連線（推送間隔預設 10 秒，
//!   連線重用的效益很小）；TLS 用 rustls（ring）＋作業系統的 CA＋`ca_file`，可帶用戶端憑證（mTLS）。
//!   不支援 gRPC（需要 HTTP/2 與 h2／tonic 的依賴；collector 一律同時提供 4318 的 HTTP）與 gzip（沒有
//!   壓縮的依賴；collector 接受未壓縮的本文）。
//! - **批次與佇列**：連線保存的每一筆 report 進入 [`super::ExportHub`] 的佇列（上限
//!   `queue_max_points`，滿了依 `drop_policy` 丟棄，`KRS-W4313`）；本工作每 `interval_secs` 秒、或佇列
//!   累積到 `max_batch_points` 時取出一批送出。
//! - **重試**：連線失敗、逾時、HTTP 429／502／503／504 重試（1、2、4… 秒，最多 30 秒；`Retry-After`
//!   優先，最多 300 秒），最多 `max_retries` 次，用完時丟棄這一批（`KRS-E4311`）；其他 4xx 不重試、
//!   直接丟棄（`KRS-E4312`）。重試期間新的 report 繼續進佇列。
//! - **resource**：每台主機一個 `ResourceMetrics`，屬性依 OpenTelemetry semantic conventions
//!   （`host.name`、`host.id`、`host.arch`、`os.type`）＋ `kairos.host.group`／`display_name`／`tags`
//!   ＋ `resource_attributes`。

use hyper::body::{Body as _, Bytes};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use log::{debug, info, warn};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};

use super::proto::Msg;
use super::settings::Effective;
use super::{CODE_OTLP_REJECTED, CODE_OTLP_SEND, ExportHub, ExportSample, lock, now_ms};
use crate::api::http::ApiBody;
use crate::config::export::{NAMING_SEMCONV, parse_attribute};
use crate::shutdown::Shutdown;
use kairos_proto::{Metric, MetricKind, standard_metric};

/// semantic conventions 的版本（`ResourceMetrics.schema_url`；`naming = "semconv"` 時）
pub const SCHEMA_URL: &str = "https://opentelemetry.io/schemas/1.26.0";
/// instrumentation scope 的名稱
pub const SCOPE_NAME: &str = "kairos";
/// 退避的上限
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// `Retry-After` 的上限
const MAX_RETRY_AFTER_SECS: u64 = 300;
/// 回應本文最多讀取的長度
const MAX_RESPONSE: usize = 64 * 1024;
/// 主機資訊（群組、顯示名稱…）重新讀取的間隔
const META_REFRESH: Duration = Duration::from_secs(60);
/// 關機時最後一次推送的期限
const FINAL_FLUSH: Duration = Duration::from_secs(2);
/// 丟棄一批的 WARN 的間隔
const DROP_WARN_EVERY: Duration = Duration::from_secs(60);

// ---- 指標名稱的對應 ----

/// 資料點的值
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Num {
    Int(i64),
    Double(f64),
}

/// 對應後的一個資料點
#[derive(Debug, Clone, PartialEq)]
pub struct Mapped {
    pub name: String,
    pub unit: String,
    /// 單調遞增的累計值（OTLP `Sum`，cumulative）；否則 `Gauge`
    pub sum: bool,
    pub attrs: Vec<(String, String)>,
    pub value: Num,
}

fn num_of(m: &Metric, scale: f64) -> Num {
    if scale == 1.0
        && let Some(i) = m.value.as_i64()
    {
        return Num::Int(i);
    }
    Num::Double(m.value_f64() * scale)
}

/// label 名稱的對應（semconv）
fn semconv_label(metric: &str, k: &str) -> String {
    match (k, metric) {
        ("mountpoint", _) => "system.filesystem.mountpoint".into(),
        ("fstype", _) => "system.filesystem.type".into(),
        ("device", m) if m.starts_with("system.network.") => "network.interface.name".into(),
        ("device", _) => "system.device".into(),
        (k, _) => k.to_string(),
    }
}

/// Kairos 的指標 → OTLP 的資料點。`semconv` 時標準指標對應 OpenTelemetry semantic conventions 的名稱、
/// 單位與屬性（沒有對應的保留 Kairos 的名稱）；自訂指標一律原樣
pub fn map_metric(m: &Metric, semconv: bool) -> Mapped {
    let std = standard_metric(&m.name).filter(|s| s.unit == m.unit);
    let counter = std.is_some_and(|s| s.kind == MetricKind::Counter);
    let raw = || Mapped {
        name: m.name.clone(),
        unit: m.unit.clone(),
        sum: counter,
        attrs: m
            .labels
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        value: num_of(m, 1.0),
    };
    if !semconv || std.is_none() {
        return raw();
    }
    // (名稱, 單位, 倍率, 額外屬性)
    let dir = |k: &str, v: &str| Some((k.to_string(), v.to_string()));
    let (name, unit, scale, extra): (&str, &str, f64, Option<(String, String)>) =
        match m.name.as_str() {
            "system.cpu.usage" => ("system.cpu.utilization", "1", 0.01, None),
            "system.cpu.load_1m" => ("system.cpu.load_average.1m", "{thread}", 1.0, None),
            "system.cpu.load_5m" => ("system.cpu.load_average.5m", "{thread}", 1.0, None),
            "system.cpu.load_15m" => ("system.cpu.load_average.15m", "{thread}", 1.0, None),
            "system.memory.total" => ("system.memory.limit", "By", 1.0, None),
            "system.memory.used" => (
                "system.memory.usage",
                "By",
                1.0,
                dir("system.memory.state", "used"),
            ),
            "system.memory.available" => ("system.linux.memory.available", "By", 1.0, None),
            "system.swap.used" => (
                "system.paging.usage",
                "By",
                1.0,
                dir("system.paging.state", "used"),
            ),
            "system.filesystem.total" => ("system.filesystem.limit", "By", 1.0, None),
            "system.filesystem.used" => (
                "system.filesystem.usage",
                "By",
                1.0,
                dir("system.filesystem.state", "used"),
            ),
            "system.filesystem.available" => (
                "system.filesystem.usage",
                "By",
                1.0,
                dir("system.filesystem.state", "free"),
            ),
            "system.filesystem.usage" => ("system.filesystem.utilization", "1", 0.01, None),
            "system.disk.read" => (
                "system.disk.io",
                "By",
                1.0,
                dir("disk.io.direction", "read"),
            ),
            "system.disk.written" => (
                "system.disk.io",
                "By",
                1.0,
                dir("disk.io.direction", "write"),
            ),
            "system.disk.read_ops" => (
                "system.disk.operations",
                "{operation}",
                1.0,
                dir("disk.io.direction", "read"),
            ),
            "system.disk.write_ops" => (
                "system.disk.operations",
                "{operation}",
                1.0,
                dir("disk.io.direction", "write"),
            ),
            "system.disk.io_time" => ("system.disk.io_time", "s", 0.001, None),
            "system.network.received" => (
                "system.network.io",
                "By",
                1.0,
                dir("network.io.direction", "receive"),
            ),
            "system.network.transmitted" => (
                "system.network.io",
                "By",
                1.0,
                dir("network.io.direction", "transmit"),
            ),
            "system.network.packets_received" => (
                "system.network.packets",
                "{packet}",
                1.0,
                dir("network.io.direction", "receive"),
            ),
            "system.network.packets_transmitted" => (
                "system.network.packets",
                "{packet}",
                1.0,
                dir("network.io.direction", "transmit"),
            ),
            "system.network.errors_received" => (
                "system.network.errors",
                "{error}",
                1.0,
                dir("network.io.direction", "receive"),
            ),
            "system.network.errors_transmitted" => (
                "system.network.errors",
                "{error}",
                1.0,
                dir("network.io.direction", "transmit"),
            ),
            "system.process.count" => ("system.process.count", "{process}", 1.0, None),
            "system.uptime" => ("system.uptime", "s", 1.0, None),
            // 置換空間總量、溫度：semantic conventions 沒有對應的指標
            _ => return raw(),
        };
    let mut attrs: Vec<(String, String)> = m
        .labels
        .iter()
        .map(|(k, v)| (semconv_label(&m.name, k), v.clone()))
        .collect();
    attrs.extend(extra);
    Mapped {
        name: name.to_string(),
        unit: unit.to_string(),
        sum: counter,
        attrs,
        value: num_of(m, scale),
    }
}

// ---- 主機資訊 ----

/// resource 屬性用的主機資訊（主機清冊）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostMeta {
    pub display_name: Option<String>,
    pub group: Option<String>,
    pub tags: Vec<String>,
    pub os_family: Option<String>,
    pub arch: Option<String>,
}

/// 讀取主機清冊（失敗時回傳空的，只記 DEBUG：resource 少了群組等屬性，指標照送）
pub fn load_host_meta(db: &Path) -> HashMap<String, HostMeta> {
    let read = || -> Result<HashMap<String, HostMeta>, String> {
        let conn = crate::store::open_read(db).map_err(|e| e.to_string())?;
        let rows = conn.host_meta_rows().map_err(|e| e.to_string())?;
        let mut out = HashMap::new();
        for (id, display, group, tags, os, family) in rows {
            let tags: Vec<String> = tags
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default();
            let os: Option<Value> = os.and_then(|o| serde_json::from_str(&o).ok());
            let arch = os
                .as_ref()
                .and_then(|o| o.get("arch"))
                .and_then(Value::as_str)
                .map(str::to_string);
            out.insert(
                id,
                HostMeta {
                    display_name: display.filter(|d| !d.is_empty()),
                    group: group.filter(|g| !g.is_empty()),
                    tags,
                    os_family: family.filter(|f| !f.is_empty()),
                    arch,
                },
            );
        }
        Ok(out)
    };
    read().unwrap_or_else(|e| {
        debug!("OTLP：無法讀取主機清冊（resource 不帶群組等屬性）：{e}");
        HashMap::new()
    })
}

/// `host.arch` 的值（semantic conventions：amd64、arm64…）
fn semconv_arch(a: &str) -> String {
    match a {
        "x86_64" | "amd64" | "x64" => "amd64".into(),
        "aarch64" | "arm64" => "arm64".into(),
        "x86" | "i386" | "i686" => "x86".into(),
        other => other.to_ascii_lowercase(),
    }
}

// ---- 編碼 ----

fn key_value(k: &str, v: &str) -> Msg {
    let mut any = Msg::new();
    any.string_always(1, v);
    let mut kv = Msg::new();
    kv.string(1, k).message(2, &any);
    kv
}

/// 一批 report → `ExportMetricsServiceRequest`。回傳 (本文, 資料點數)
pub fn encode(
    batch: &[Arc<ExportSample>],
    meta: &HashMap<String, HostMeta>,
    eff: &Effective,
) -> (Vec<u8>, usize) {
    let semconv = eff.otlp.naming == NAMING_SEMCONV;
    let extra: Vec<(String, String)> = eff
        .otlp
        .resource_attributes
        .iter()
        .filter_map(|a| parse_attribute(a).ok())
        .collect();
    // 依主機分組（保留第一次出現的順序）
    let mut order: Vec<&str> = Vec::new();
    let mut by_host: HashMap<&str, Vec<&ExportSample>> = HashMap::new();
    for s in batch {
        let e = by_host.entry(&s.agent_id).or_default();
        if e.is_empty() {
            order.push(&s.agent_id);
        }
        e.push(s);
    }
    let mut points = 0usize;
    let mut req = Msg::new();
    for agent in order {
        let samples = &by_host[agent];
        let first = samples[0];
        let m = meta.get(agent).cloned().unwrap_or_default();
        let mut res = Msg::new();
        res.message(1, &key_value("host.name", &first.hostname))
            .message(1, &key_value("host.id", agent));
        if let Some(a) = &m.arch {
            res.message(1, &key_value("host.arch", &semconv_arch(a)));
        }
        if let Some(f) = &m.os_family {
            res.message(1, &key_value("os.type", &f.to_ascii_lowercase()));
        }
        if let Some(g) = &m.group {
            res.message(1, &key_value("kairos.host.group", g));
        }
        if let Some(d) = &m.display_name {
            res.message(1, &key_value("kairos.host.display_name", d));
        }
        if !m.tags.is_empty() {
            res.message(1, &key_value("kairos.host.tags", &m.tags.join(",")));
        }
        for (k, v) in &extra {
            res.message(1, &key_value(k, v));
        }
        // (名稱, 單位, sum) → 資料點
        let mut idx: HashMap<(String, String, bool), usize> = HashMap::new();
        let mut metrics: Vec<((String, String, bool), Vec<Msg>)> = Vec::new();
        for s in samples {
            let ts_ns = u64::try_from(s.ts_ms)
                .unwrap_or(0)
                .saturating_mul(1_000_000);
            let start_ns = s
                .uptime_secs()
                .and_then(|up| {
                    u64::try_from(s.ts_ms)
                        .ok()?
                        .checked_sub(up.saturating_mul(1000))
                })
                .map(|ms| ms.saturating_mul(1_000_000))
                .unwrap_or(0);
            for metric in &s.metrics {
                let mp = map_metric(metric, semconv);
                let mut dp = Msg::new();
                for (k, v) in &mp.attrs {
                    dp.message(7, &key_value(k, v));
                }
                if mp.sum {
                    dp.fixed64(2, start_ns);
                }
                dp.fixed64(3, ts_ns);
                match mp.value {
                    Num::Double(v) => dp.double_always(4, v),
                    Num::Int(v) => dp.sfixed64_always(6, v),
                };
                let key = (mp.name, mp.unit, mp.sum);
                let i = *idx.entry(key.clone()).or_insert_with(|| {
                    metrics.push((key, Vec::new()));
                    metrics.len() - 1
                });
                metrics[i].1.push(dp);
                points += 1;
            }
        }
        let mut scope = Msg::new();
        let mut is = Msg::new();
        is.string(1, SCOPE_NAME)
            .string(2, env!("CARGO_PKG_VERSION"));
        scope.message(1, &is);
        for ((name, unit, sum), dps) in metrics {
            let mut data = Msg::new();
            for dp in &dps {
                data.message(1, dp);
            }
            let mut metric = Msg::new();
            metric.string(1, &name).string(3, &unit);
            if sum {
                data.uint(2, 2) // AGGREGATION_TEMPORALITY_CUMULATIVE
                    .boolean(3, true);
                metric.message(7, &data);
            } else {
                metric.message(5, &data);
            }
            scope.message(2, &metric);
        }
        let mut rm = Msg::new();
        rm.message(1, &res).message(2, &scope);
        if semconv {
            rm.string(3, SCHEMA_URL);
        }
        req.message(1, &rm);
    }
    (req.into_bytes(), points)
}

// ---- HTTP 用戶端 ----

/// 一次請求的結果
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub ok: bool,
    /// 失敗時是否值得重試
    pub retryable: bool,
    pub status: Option<u16>,
    pub code: Option<&'static str>,
    pub message: String,
    pub retry_after_secs: Option<u64>,
    /// 回應 `partial_success.rejected_data_points`
    pub rejected_points: u64,
    pub elapsed_ms: u64,
}

impl Outcome {
    fn fail(retryable: bool, code: &'static str, message: String, started: Instant) -> Self {
        Self {
            ok: false,
            retryable,
            status: None,
            code: Some(code),
            message,
            retry_after_secs: None,
            rejected_points: 0,
            elapsed_ms: started.elapsed().as_millis() as u64,
        }
    }
}

fn native_roots() -> &'static Vec<CertificateDer<'static>> {
    static ROOTS: OnceLock<Vec<CertificateDer<'static>>> = OnceLock::new();
    ROOTS.get_or_init(|| rustls_native_certs::load_native_certs().certs)
}

/// TLS 設定：作業系統的 CA ＋ `ca_file`，`client_cert` 設定時帶用戶端憑證
pub fn tls_config(eff: &Effective) -> Result<Arc<ClientConfig>, String> {
    let o = &eff.otlp;
    let mut roots = RootCertStore::empty();
    for c in native_roots() {
        let _ = roots.add(c.clone());
    }
    let ca = o.ca_file.trim();
    if !ca.is_empty() {
        let certs = CertificateDer::pem_file_iter(ca)
            .and_then(|it| it.collect::<Result<Vec<_>, _>>())
            .map_err(|e| format!("無法讀取 export.otlp.ca_file {ca}：{e}"))?;
        if certs.is_empty() {
            return Err(format!("export.otlp.ca_file {ca} 中沒有憑證"));
        }
        for c in certs {
            roots
                .add(c)
                .map_err(|e| format!("export.otlp.ca_file {ca} 的憑證無法使用：{e}"))?;
        }
    }
    let builder =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots);
    let (cert, key) = (o.client_cert.trim(), o.client_key.trim());
    let config = if cert.is_empty() {
        builder.with_no_client_auth()
    } else {
        let chain = CertificateDer::pem_file_iter(cert)
            .and_then(|it| it.collect::<Result<Vec<_>, _>>())
            .map_err(|e| format!("無法讀取 export.otlp.client_cert {cert}：{e}"))?;
        let key = PrivateKeyDer::from_pem_file(key)
            .map_err(|e| format!("無法讀取 export.otlp.client_key {key}：{e}"))?;
        builder
            .with_client_auth_cert(chain, key)
            .map_err(|e| format!("export.otlp.client_cert／client_key 無法使用：{e}"))?
    };
    Ok(Arc::new(config))
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// 回應本文的 `partial_success`：(被拒的資料點, 訊息)
fn partial_success(body: &[u8]) -> (u64, String) {
    let Ok(f) = super::proto::decode(body) else {
        return (0, String::new());
    };
    let Some(ps) = super::proto::field(&f, 1) else {
        return (0, String::new());
    };
    let Ok(p) = super::proto::decode(ps.bytes()) else {
        return (0, String::new());
    };
    (
        super::proto::field(&p, 1).map_or(0, |v| v.u64()),
        super::proto::field(&p, 2)
            .map(|v| v.str().to_string())
            .unwrap_or_default(),
    )
}

/// 錯誤訊息中不出現標頭的值
fn scrub(text: &str, eff: &Effective) -> String {
    let mut t = text.to_string();
    for (_, v) in &eff.headers {
        if v.len() >= 4 {
            t = t.replace(v.as_str(), crate::alerts::secrets::MASK);
        }
    }
    t
}

/// 送出一個請求
pub async fn post(eff: &Effective, body: Vec<u8>) -> Outcome {
    let started = Instant::now();
    let o = &eff.otlp;
    let target = match crate::alerts::notify::transport::parse_url(&o.endpoint, true) {
        Ok(t) => t,
        Err(e) => {
            return Outcome::fail(
                false,
                super::CODE_EXPORT_CONFIG,
                format!("OTLP 端點無效：{e}"),
                started,
            );
        }
    };
    let timeout = Duration::from_secs(o.timeout_secs.max(1));
    let tls = if target.https {
        match tls_config(eff) {
            Ok(c) => Some(c),
            Err(e) => return Outcome::fail(false, super::CODE_EXPORT_CONFIG, e, started),
        }
    } else {
        None
    };
    let host = target.host.clone();
    let result = tokio::time::timeout(timeout, async {
        let tcp = TcpStream::connect((target.host.as_str(), target.port))
            .await
            .map_err(|e| format!("無法連線到 {host}:{}：{e}", target.port))?;
        let _ = tcp.set_nodelay(true);
        let io: Box<dyn Io> = match tls {
            None => Box::new(tcp),
            Some(cfg) => {
                let name = ServerName::try_from(target.host.clone())
                    .map_err(|e| format!("主機名稱 {host} 無效：{e}"))?;
                let s = TlsConnector::from(cfg)
                    .connect(name, tcp)
                    .await
                    .map_err(|e| {
                        let t = e.to_string();
                        if t.contains("UnknownIssuer") || t.contains("certificate") {
                            format!("{host} 的伺服器憑證不受信任（{t}）：確認 export.otlp.ca_file")
                        } else {
                            format!("與 {host} 的 TLS 握手失敗：{t}")
                        }
                    })?;
                Box::new(s)
            }
        };
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
            .await
            .map_err(|e| format!("與 {host} 的 HTTP 連線失敗：{e}"))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let host_header =
            if (target.https && target.port == 443) || (!target.https && target.port == 80) {
                target.host.clone()
            } else if target.host.contains(':') {
                format!("[{}]:{}", target.host, target.port)
            } else {
                format!("{}:{}", target.host, target.port)
            };
        let mut req = Request::builder()
            .method(Method::POST)
            .uri(&target.path)
            .header(hyper::header::HOST, host_header)
            .header(hyper::header::CONTENT_TYPE, "application/x-protobuf")
            .header(
                hyper::header::USER_AGENT,
                format!("Kairos/{}", env!("CARGO_PKG_VERSION")),
            )
            .header(hyper::header::CONTENT_LENGTH, body.len());
        for (k, v) in &eff.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let req = req
            .body(ApiBody::Full(Some(Bytes::from(body))))
            .map_err(|e| format!("無法建立請求：{e}"))?;
        let resp = sender
            .send_request(req)
            .await
            .map_err(|e| format!("送出到 {host} 失敗：{e}"))?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get(hyper::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let is_proto = resp
            .headers()
            .get(hyper::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/x-protobuf"));
        let mut body = resp.into_body();
        let mut buf = Vec::new();
        while buf.len() < MAX_RESPONSE {
            match std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)).await {
                Some(Ok(f)) => {
                    if let Ok(d) = f.into_data() {
                        buf.extend_from_slice(&d);
                    }
                }
                _ => break,
            }
        }
        Ok::<_, String>((status, retry_after, is_proto, buf))
    })
    .await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match result {
        Err(_) => Outcome::fail(
            true,
            CODE_OTLP_SEND,
            format!("{} 在 {} 秒內沒有回應", target.host, timeout.as_secs()),
            started,
        ),
        Ok(Err(e)) => Outcome::fail(true, CODE_OTLP_SEND, scrub(&e, eff), started),
        Ok(Ok((status, retry_after, is_proto, buf))) => {
            if (200..300).contains(&status) {
                let (rejected, msg) = if is_proto {
                    partial_success(&buf)
                } else {
                    (0, String::new())
                };
                return Outcome {
                    ok: true,
                    retryable: false,
                    status: Some(status),
                    code: (rejected > 0).then_some(CODE_OTLP_REJECTED),
                    message: if rejected > 0 {
                        format!("collector 拒絕 {rejected} 個資料點：{}", scrub(&msg, eff))
                    } else {
                        format!("HTTP {status}")
                    },
                    retry_after_secs: None,
                    rejected_points: rejected,
                    elapsed_ms,
                };
            }
            let retryable = matches!(status, 429 | 502 | 503 | 504);
            let detail = if is_proto {
                // OTLP 的錯誤本文是 google.rpc.Status（2：message）
                super::proto::decode(&buf)
                    .ok()
                    .and_then(|f| super::proto::field(&f, 2).map(|v| v.str().to_string()))
                    .unwrap_or_default()
            } else {
                String::from_utf8_lossy(&buf).chars().take(300).collect()
            };
            Outcome {
                ok: false,
                retryable,
                status: Some(status),
                code: Some(if retryable {
                    CODE_OTLP_SEND
                } else {
                    CODE_OTLP_REJECTED
                }),
                message: scrub(
                    &format!(
                        "{} 回應 HTTP {status}{}",
                        target.host,
                        if detail.trim().is_empty() {
                            String::new()
                        } else {
                            format!("：{}", detail.trim())
                        }
                    ),
                    eff,
                ),
                retry_after_secs: retry_after
                    .filter(|_| status == 429 || status == 503)
                    .map(|s| s.min(MAX_RETRY_AFTER_SECS)),
                rejected_points: 0,
                elapsed_ms,
            }
        }
    }
}

/// 測試連線：送出一個空的 `ExportMetricsServiceRequest`（collector 回 2xx 即成功，不寫入任何資料）
pub async fn test_connection(eff: &Effective) -> Outcome {
    post(eff, Vec::new()).await
}

// ---- 推送工作 ----

/// 記錄一次請求的結果
fn record(hub: &ExportHub, out: &Outcome, points: usize, bytes: usize) {
    let now = now_ms();
    let mut st = lock(&hub.otlp_status);
    st.last_attempt_ms = Some(now);
    if out.ok {
        hub.stats.otlp_requests_ok.fetch_add(1, Ordering::Relaxed);
        hub.stats.otlp_sent_points.fetch_add(
            (points as u64).saturating_sub(out.rejected_points),
            Ordering::Relaxed,
        );
        hub.stats
            .otlp_dropped_rejected
            .fetch_add(out.rejected_points, Ordering::Relaxed);
        if st.consecutive_failures > 0 {
            info!(
                event = "export.otlp_recovered";
                "OTLP 推送已恢復（之前連續失敗 {} 次）",
                st.consecutive_failures
            );
        }
        st.consecutive_failures = 0;
        st.last_success_ms = Some(now);
        st.last_request_ms = out.elapsed_ms;
        st.last_request_bytes = bytes;
        if out.rejected_points > 0 {
            st.last_error = Some((CODE_OTLP_REJECTED, out.message.clone(), now));
            warn!(
                event = "export.otlp_partial", code = CODE_OTLP_REJECTED;
                "警告 {CODE_OTLP_REJECTED}：{}", out.message
            );
        }
    } else {
        hub.stats
            .otlp_requests_failed
            .fetch_add(1, Ordering::Relaxed);
        st.consecutive_failures += 1;
        let code = out.code.unwrap_or(CODE_OTLP_SEND);
        st.last_error = Some((code, out.message.clone(), now));
        if st.consecutive_failures == 1 {
            warn!(
                event = "export.otlp_failed", code = code;
                "錯誤 {code}：OTLP 推送失敗：{}{}",
                out.message,
                if out.retryable { "（稍後重試）" } else { "（不重試，這一批丟棄）" }
            );
        } else {
            debug!(
                "OTLP 推送失敗（連續第 {} 次）：{}",
                st.consecutive_failures, out.message
            );
        }
    }
}

/// 退避時間：`Retry-After` 優先，否則 1、2、4… 秒，最多 30 秒
fn backoff(attempt: u32, retry_after: Option<u64>) -> Duration {
    if let Some(s) = retry_after {
        return Duration::from_secs(s.clamp(1, MAX_RETRY_AFTER_SECS));
    }
    Duration::from_secs(1u64 << attempt.min(5)).min(MAX_BACKOFF)
}

/// 送出一批（含重試）；回傳時這一批已送出或已丟棄
async fn send_batch(
    hub: &ExportHub,
    batch: &[Arc<ExportSample>],
    meta: &HashMap<String, HostMeta>,
    shutdown: &Shutdown,
    last_drop_warn: &mut Option<Instant>,
) {
    let mut attempt = 0u32;
    loop {
        let eff = hub.effective();
        if !eff.otlp_active() {
            return;
        }
        let (body, points) = encode(batch, meta, &eff);
        let bytes = body.len();
        let out = post(&eff, body).await;
        record(hub, &out, points, bytes);
        if out.ok {
            return;
        }
        let give_up = !out.retryable || attempt >= eff.otlp.max_retries;
        if give_up {
            let counter = if out.retryable {
                &hub.stats.otlp_dropped_failed
            } else {
                &hub.stats.otlp_dropped_rejected
            };
            counter.fetch_add(points as u64, Ordering::Relaxed);
            if last_drop_warn.is_none_or(|t| t.elapsed() >= DROP_WARN_EVERY) {
                *last_drop_warn = Some(Instant::now());
                let code = out.code.unwrap_or(CODE_OTLP_SEND);
                warn!(
                    event = "export.otlp_dropped", code = code;
                    "錯誤 {code}：OTLP 推送{}，丟棄這一批 {points} 個資料點：{}（每分鐘最多記錄一次；累計見 /metrics 的 kairos_export_otlp_dropped_points_total）",
                    if out.retryable { format!("重試 {} 次仍失敗", eff.otlp.max_retries) } else { "被 collector 拒絕".to_string() },
                    out.message
                );
            }
            return;
        }
        let wait = backoff(attempt, out.retry_after_secs);
        attempt += 1;
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                hub.stats.otlp_dropped_failed.fetch_add(points as u64, Ordering::Relaxed);
                return;
            }
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

/// OTLP 推送工作（Kairos 執行期間一直存在；OTLP 停用時只等待設定改變）
pub async fn run(hub: Arc<ExportHub>, shutdown: Arc<Shutdown>) {
    let mut rx = hub.subscribe();
    let mut meta: HashMap<String, HostMeta> = HashMap::new();
    let mut meta_at: Option<Instant> = None;
    let mut last_drop_warn = None;
    loop {
        let eff = rx.borrow_and_update().clone();
        if !eff.otlp_active() {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                r = rx.changed() => if r.is_err() { return },
            }
            continue;
        }
        if hub.queue_points() < eff.otlp.max_batch_points {
            let interval = Duration::from_secs(eff.otlp.interval_secs.max(1));
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    final_flush(&hub, &meta).await;
                    return;
                }
                r = rx.changed() => {
                    if r.is_err() { return }
                    continue;
                }
                _ = tokio::time::sleep(interval) => {}
                _ = hub.notified() => {}
            }
        }
        if let Some(db) = hub.db_path.clone()
            && meta_at.is_none_or(|t| t.elapsed() >= META_REFRESH)
        {
            meta = tokio::task::spawn_blocking(move || load_host_meta(&db))
                .await
                .unwrap_or_default();
            meta_at = Some(Instant::now());
        }
        let batch = hub.take_batch(eff.otlp.max_batch_points);
        if batch.is_empty() {
            continue;
        }
        send_batch(&hub, &batch, &meta, &shutdown, &mut last_drop_warn).await;
        if shutdown.started_at().is_some() {
            final_flush(&hub, &meta).await;
            return;
        }
    }
}

/// 關機時把佇列中的資料再送一次（最多 [`FINAL_FLUSH`]，不重試）
async fn final_flush(hub: &ExportHub, meta: &HashMap<String, HostMeta>) {
    let eff = hub.effective();
    if !eff.otlp_active() {
        return;
    }
    let batch = hub.take_batch(eff.otlp.max_batch_points);
    if batch.is_empty() {
        return;
    }
    let (body, points) = encode(&batch, meta, &eff);
    let bytes = body.len();
    match tokio::time::timeout(FINAL_FLUSH, post(&eff, body)).await {
        Ok(out) => record(hub, &out, points, bytes),
        Err(_) => {
            hub.stats
                .otlp_dropped_failed
                .fetch_add(points as u64, Ordering::Relaxed);
        }
    }
    let left = hub.queue_points();
    if left > 0 {
        hub.stats
            .otlp_dropped_failed
            .fetch_add(left as u64, Ordering::Relaxed);
        info!("關機：OTLP 佇列中還有 {left} 個資料點未送出（已丟棄）");
    }
}
