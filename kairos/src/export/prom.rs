//! Prometheus exporter 的輸出：每台主機最新的一筆 report 轉成 Prometheus text 0.0.4 或
//! OpenMetrics 1.0（`kairos/EXPORT.md` 第 2 節）。
//!
//! - **名稱**：Kairos 的名稱（`system.cpu.usage`）的 `.` 換成 `_`，依 OpenTelemetry 轉 Prometheus 的慣例加上
//!   單位後綴（`By` → `_bytes`、`s` → `_seconds`、`%` → `_percent`、`Cel` → `_celsius`；`ms` 換算成秒；
//!   `1` 與 `{packets}` 這類標註不加），計數器另加 `_total`。例：`system_cpu_usage_percent`、
//!   `system_network_received_bytes_total`、`system_disk_io_time_seconds_total`。
//! - **標籤**：每個序列都帶 `agent_id`、`hostname`、`host_group`（有群組時）＋指標本身的標籤
//!   （`mountpoint`、`device`…）。指標的標籤與這三個重名時改為 `exported_<名稱>`。顯示名稱、標籤（tags）、
//!   版本與作業系統只放在 `kairos_host_info`（值為 1，以 `group_left` 併入），避免每個序列都帶高變動的標籤。
//! - **離線與過期**：`kairos_host_up` 一律輸出（線上 1、其他 0）；離線、或最後一筆超過
//!   `stale_after_secs` 的主機不輸出指標（Prometheus 依 staleness 規則在 5 分鐘內把序列標為過期），
//!   不帶時間戳記（樣本時間為抓取時間；`kairos_host_last_sample_timestamp_seconds` 是取樣時間）。
//! - **行程排行**：預設不輸出（標籤含 pid 與行程名稱，行程重啟就是新序列）；啟用時每台主機 CPU 與記憶體
//!   各前 N 名。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;

use super::ExportSample;
use kairos_proto::{MetricKind, standard_metric};

/// 一台主機（主控台的主機索引＋最新的一筆 report）
#[derive(Debug, Clone)]
pub struct HostRow {
    pub agent_id: Arc<str>,
    pub hostname: Arc<str>,
    pub display_name: Option<Arc<str>>,
    pub group: Option<Arc<str>>,
    /// 線上（主機索引的狀態為 `online`）
    pub up: bool,
    pub agent_version: Option<Arc<str>>,
    pub os_family: Option<String>,
    pub tags: Arc<[String]>,
    pub sample: Option<Arc<ExportSample>>,
}

/// 輸出選項
#[derive(Debug, Clone)]
pub struct Options {
    pub now_ms: i64,
    pub stale_after_ms: i64,
    pub include_processes: bool,
    pub processes_top_n: usize,
    pub openmetrics: bool,
}

/// 輸出結果
#[derive(Debug)]
pub struct Rendered {
    pub body: String,
    pub hosts: usize,
    /// 輸出的指標序列數（不含 `kairos_host_*`、`kairos_exporter_*`）
    pub series: usize,
}

/// text 0.0.4 的 Content-Type
pub const CONTENT_TYPE_TEXT: &str = "text/plain; version=0.0.4; charset=utf-8";
/// OpenMetrics 1.0 的 Content-Type
pub const CONTENT_TYPE_OPENMETRICS: &str =
    "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// 依 `Accept` 標頭決定格式：明確接受 `application/openmetrics-text` 時用 OpenMetrics
pub fn wants_openmetrics(accept: Option<&str>) -> bool {
    accept.is_some_and(|a| {
        a.split(',').any(|part| {
            let mut it = part.split(';');
            let ty = it.next().unwrap_or("").trim();
            let q_zero = it.any(|p| {
                p.trim()
                    .strip_prefix("q=")
                    .and_then(|q| q.trim().parse::<f64>().ok())
                    == Some(0.0)
            });
            ty.eq_ignore_ascii_case("application/openmetrics-text") && !q_zero
        })
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FamKind {
    Gauge,
    Counter,
    Unknown,
}

#[derive(Debug)]
struct Family {
    kind: FamKind,
    help: String,
    lines: String,
}

/// 依名稱排序後輸出的 family 集合
#[derive(Debug, Default)]
struct Families {
    map: HashMap<String, Family>,
}

impl Families {
    /// 取得（或建立）family；種類與既有的不同時回 `None`（略過這個樣本）
    fn get(
        &mut self,
        base: &str,
        kind: FamKind,
        help: impl FnOnce() -> String,
    ) -> Option<&mut Family> {
        let f = self.map.entry(base.to_string()).or_insert_with(|| Family {
            kind,
            help: help(),
            lines: String::new(),
        });
        (f.kind == kind).then_some(f)
    }
}

/// 名稱中不合法的字元換成 `_`；開頭是數字時加 `_`
fn sanitize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 1);
    for (i, c) in name.chars().enumerate() {
        if i == 0 && c.is_ascii_digit() {
            out.push('_');
        }
        out.push(if c.is_ascii_alphanumeric() || c == '_' {
            c
        } else {
            '_'
        });
    }
    out
}

/// 單位 → (後綴, 倍率)
fn unit_suffix(unit: &str) -> (Option<String>, f64) {
    match unit {
        "By" => (Some("bytes".into()), 1.0),
        "s" => (Some("seconds".into()), 1.0),
        "ms" => (Some("seconds".into()), 0.001),
        "%" => (Some("percent".into()), 1.0),
        "Cel" => (Some("celsius".into()), 1.0),
        "By/s" => (Some("bytes_per_second".into()), 1.0),
        "1" | "" => (None, 1.0),
        u if u.starts_with('{') && u.ends_with('}') => (None, 1.0),
        u => {
            let s = sanitize_name(&u.replace('/', "_per_").to_ascii_lowercase());
            let s = s.trim_matches('_').to_string();
            ((!s.is_empty()).then_some(s), 1.0)
        }
    }
}

/// Kairos 的指標 → (Prometheus family 名稱（計數器不含 `_total`）, 倍率)
pub fn prom_name(name: &str, unit: &str) -> (String, f64) {
    let mut base = sanitize_name(name);
    let (suffix, scale) = unit_suffix(unit);
    if let Some(s) = suffix
        && !base.ends_with(&format!("_{s}"))
    {
        base.push('_');
        base.push_str(&s);
    }
    // 自訂指標不可冒用 exporter 自己的名稱
    if ["kairos_host_", "kairos_process_", "kairos_exporter_"]
        .iter()
        .any(|p| base.starts_with(p))
    {
        base.insert_str(0, "custom_");
    }
    (base, scale)
}

fn escape_into(v: &str, out: &mut String) {
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

fn label(out: &mut String, k: &str, v: &str) {
    if !out.is_empty() {
        out.push(',');
    }
    out.push_str(k);
    out.push_str("=\"");
    escape_into(v, out);
    out.push('"');
}

/// 標籤名稱：不合法的字元換成 `_`，`__` 開頭（保留給 Prometheus）加前綴
fn label_name(k: &str) -> String {
    let s = sanitize_name(k);
    if s.starts_with("__") {
        format!("label{s}")
    } else {
        s
    }
}

const HOST_LABELS: [&str; 3] = ["agent_id", "hostname", "host_group"];

fn host_labels(h: &HostRow) -> String {
    let mut s = String::new();
    label(&mut s, "agent_id", &h.agent_id);
    label(&mut s, "hostname", &h.hostname);
    if let Some(g) = h.group.as_deref().filter(|g| !g.is_empty()) {
        label(&mut s, "host_group", g);
    }
    s
}

fn fmt_f64(v: f64) -> String {
    kp_obs::prom::format_value(v)
}

fn line(out: &mut String, name: &str, labels: &str, value: &str) {
    out.push_str(name);
    if !labels.is_empty() {
        out.push('{');
        out.push_str(labels);
        out.push('}');
    }
    out.push(' ');
    out.push_str(value);
    out.push('\n');
}

/// 產生輸出
pub fn render(rows: &[HostRow], o: &Options) -> Rendered {
    let mut fams = Families::default();
    let mut up = String::new();
    let mut info = String::new();
    let mut last = String::new();
    let (mut fresh, mut stale, mut offline, mut no_data) = (0usize, 0usize, 0usize, 0usize);
    let mut series = 0usize;
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for h in rows {
        let hl = host_labels(h);
        line(&mut up, "kairos_host_up", &hl, if h.up { "1" } else { "0" });
        let mut il = hl.clone();
        if let Some(d) = h.display_name.as_deref().filter(|d| !d.is_empty()) {
            label(&mut il, "display_name", d);
        }
        if let Some(v) = h.agent_version.as_deref().filter(|v| !v.is_empty()) {
            label(&mut il, "agent_version", v);
        }
        if let Some(f) = h.os_family.as_deref().filter(|f| !f.is_empty()) {
            label(&mut il, "os_family", f);
        }
        if !h.tags.is_empty() {
            let mut t: Vec<&str> = h.tags.iter().map(String::as_str).collect();
            t.sort_unstable();
            label(&mut il, "tags", &t.join(","));
        }
        line(&mut info, "kairos_host_info", &il, "1");
        let Some(s) = &h.sample else {
            if h.up {
                no_data += 1;
            } else {
                offline += 1;
            }
            continue;
        };
        line(
            &mut last,
            "kairos_host_last_sample_timestamp_seconds",
            &hl,
            &fmt_f64(s.ts_ms as f64 / 1000.0),
        );
        if !h.up {
            offline += 1;
            continue;
        }
        if o.now_ms - s.ts_ms > o.stale_after_ms {
            stale += 1;
            continue;
        }
        fresh += 1;
        seen.clear();
        for m in &s.metrics {
            let std = standard_metric(&m.name).filter(|sm| sm.unit == m.unit);
            let kind = match std.map(|sm| sm.kind) {
                Some(MetricKind::Counter) => FamKind::Counter,
                Some(MetricKind::Gauge) => FamKind::Gauge,
                None => FamKind::Unknown,
            };
            let (base, scale) = prom_name(&m.name, &m.unit);
            let mut labels = hl.clone();
            for (k, v) in &m.labels {
                let k = label_name(k);
                let k = if HOST_LABELS.contains(&k.as_str()) {
                    format!("exported_{k}")
                } else {
                    k
                };
                label(&mut labels, &k, v);
            }
            if !seen.insert((base.clone(), labels.clone())) {
                continue;
            }
            let value = if scale == 1.0 {
                m.value.to_string()
            } else {
                fmt_f64(m.value_f64() * scale)
            };
            let (name, unit) = (m.name.as_str(), m.unit.as_str());
            let Some(f) = fams.get(&base, kind, || {
                if std.is_some() {
                    format!("Pulse 標準指標 {name}（單位 {unit}；crates/kairos-proto/METRICS.md）")
                } else {
                    format!("Pulse 回報的指標 {name}（單位 {unit}）")
                }
            }) else {
                continue;
            };
            let sample_name = if kind == FamKind::Counter {
                format!("{base}_total")
            } else {
                base.clone()
            };
            line(&mut f.lines, &sample_name, &labels, &value);
            series += 1;
        }
        if o.include_processes
            && let Some(p) = &s.processes
        {
            let n = o.processes_top_n;
            for e in p.top_cpu.iter().take(n) {
                let mut l = hl.clone();
                label(&mut l, "pid", &e.pid.to_string());
                label(&mut l, "process", &e.name);
                if let Some(f) =
                    fams.get("kairos_process_cpu_usage_percent", FamKind::Gauge, || {
                        "行程排行：CPU 使用率最高的行程占整台主機的 CPU（%）".into()
                    })
                {
                    line(
                        &mut f.lines,
                        "kairos_process_cpu_usage_percent",
                        &l,
                        &fmt_f64(e.cpu_usage),
                    );
                    series += 1;
                }
            }
            for e in p.top_memory.iter().take(n) {
                let mut l = hl.clone();
                label(&mut l, "pid", &e.pid.to_string());
                label(&mut l, "process", &e.name);
                if let Some(f) = fams.get("kairos_process_memory_bytes", FamKind::Gauge, || {
                    "行程排行：常駐記憶體最高的行程（bytes；Linux RSS、Windows Working Set）".into()
                }) {
                    line(
                        &mut f.lines,
                        "kairos_process_memory_bytes",
                        &l,
                        &e.memory.to_string(),
                    );
                    series += 1;
                }
            }
        }
    }

    let mut out = String::with_capacity(
        up.len() * 2
            + info.len()
            + last.len()
            + fams
                .map
                .values()
                .map(|f| f.lines.len() + 128)
                .sum::<usize>()
            + 1024,
    );
    let om = o.openmetrics;
    let header = |out: &mut String, base: &str, kind: FamKind, help: &str| {
        let ty = match (kind, om) {
            (FamKind::Gauge, _) => "gauge",
            (FamKind::Counter, _) => "counter",
            (FamKind::Unknown, true) => "unknown",
            (FamKind::Unknown, false) => "untyped",
        };
        // text 0.0.4 的計數器以樣本名稱（含 _total）宣告；OpenMetrics 以 family 名稱（不含 _total）
        let name = if kind == FamKind::Counter && !om {
            format!("{base}_total")
        } else {
            base.to_string()
        };
        let _ = writeln!(
            out,
            "# HELP {name} {}",
            help.replace('\\', "\\\\").replace('\n', "\\n")
        );
        let _ = writeln!(out, "# TYPE {name} {ty}");
    };
    header(
        &mut out,
        "kairos_host_up",
        FamKind::Gauge,
        "主機是否線上（1：與 Kairos 連線中；0：離線或尚未連線）",
    );
    out.push_str(&up);
    header(
        &mut out,
        "kairos_host_info",
        FamKind::Gauge,
        "主機的描述資訊（值固定為 1）：顯示名稱、Pulse 版本、作業系統、標籤；以 group_left 併入其他序列",
    );
    out.push_str(&info);
    header(
        &mut out,
        "kairos_host_last_sample_timestamp_seconds",
        FamKind::Gauge,
        "主機最後一筆 report 的取樣時間（Unix 秒；Kairos 這次啟動後收到的）",
    );
    out.push_str(&last);
    header(
        &mut out,
        "kairos_exporter_hosts",
        FamKind::Gauge,
        "依狀態的主機數（fresh：輸出指標；stale：最後一筆超過 stale_after_secs；offline：離線；no_data：線上但這次啟動後還沒有 report）",
    );
    for (st, n) in [
        ("fresh", fresh),
        ("stale", stale),
        ("offline", offline),
        ("no_data", no_data),
    ] {
        let _ = writeln!(out, "kairos_exporter_hosts{{state=\"{st}\"}} {n}");
    }
    let sorted: BTreeMap<&String, &Family> = fams.map.iter().collect();
    for (base, f) in sorted {
        header(&mut out, base, f.kind, &f.help);
        out.push_str(&f.lines);
    }
    if om {
        out.push_str("# EOF\n");
    }
    Rendered {
        body: out,
        hosts: rows.len(),
        series,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kairos_proto::{Metric, ProcessEntry, ProcessList};

    pub(crate) fn sample(agent: &str, ts_ms: i64) -> Arc<ExportSample> {
        Arc::new(ExportSample {
            agent_id: Arc::from(agent),
            hostname: Arc::from("web-01"),
            ts_ms,
            metrics: vec![
                Metric::new(
                    "system.cpu.usage",
                    serde_json::Number::from_f64(12.5).unwrap(),
                    "%",
                ),
                Metric::new("system.memory.total", 8_589_934_592u64, "By"),
                Metric::new(
                    "system.filesystem.usage",
                    serde_json::Number::from_f64(41.25).unwrap(),
                    "%",
                )
                .with_label("mountpoint", "/")
                .with_label("fstype", "ext4"),
                Metric::new(
                    "system.network.received",
                    18_446_744_073_709_551_000u64,
                    "By",
                )
                .with_label("device", "eth0"),
                Metric::new("system.disk.io_time", 12_345u64, "ms").with_label("device", "sda"),
                Metric::new(
                    "system.cpu.load_1m",
                    serde_json::Number::from_f64(0.5).unwrap(),
                    "1",
                ),
                Metric::new("system.network.packets_received", 42u64, "{packets}")
                    .with_label("device", "eth0"),
                Metric::new("system.uptime", 3600u64, "s"),
                // 自訂指標：標籤與主機標籤重名、名稱冒用 exporter 的前綴
                Metric::new("app.queue.depth", 7u64, "{items}").with_label("hostname", "x\"y\\z"),
                Metric::new("kairos.host.up", 1u64, "1"),
                // 單位與標準不同：當作自訂（unknown）
                Metric::new("system.temperature", 70u64, "K").with_label("sensor", "cpu"),
            ],
            processes: Some(ProcessList {
                top_cpu: vec![ProcessEntry {
                    pid: 42,
                    name: "postgres".into(),
                    cpu_usage: 3.5,
                    memory: 1024,
                    user: None,
                    command: None,
                }],
                top_memory: vec![],
            }),
        })
    }

    fn row(agent: &str, up: bool, s: Option<Arc<ExportSample>>) -> HostRow {
        HostRow {
            agent_id: Arc::from(agent),
            hostname: Arc::from("web-01"),
            display_name: Some(Arc::from("網頁 01")),
            group: Some(Arc::from("B廠")),
            up,
            agent_version: Some(Arc::from("0.8.0")),
            os_family: Some("linux".into()),
            tags: Arc::from(vec!["web".to_string(), "env:prod".to_string()]),
            sample: s,
        }
    }

    fn opts(now: i64) -> Options {
        Options {
            now_ms: now,
            stale_after_ms: 300_000,
            include_processes: false,
            processes_top_n: 5,
            openmetrics: false,
        }
    }

    #[test]
    fn names_follow_the_otel_to_prometheus_convention() {
        assert_eq!(
            prom_name("system.cpu.usage", "%").0,
            "system_cpu_usage_percent"
        );
        assert_eq!(
            prom_name("system.memory.total", "By").0,
            "system_memory_total_bytes"
        );
        assert_eq!(prom_name("system.uptime", "s").0, "system_uptime_seconds");
        assert_eq!(
            prom_name("system.disk.io_time", "ms"),
            ("system_disk_io_time_seconds".into(), 0.001)
        );
        assert_eq!(prom_name("system.cpu.load_1m", "1").0, "system_cpu_load_1m");
        assert_eq!(
            prom_name("system.network.packets_received", "{packets}").0,
            "system_network_packets_received"
        );
        assert_eq!(
            prom_name("system.temperature", "Cel").0,
            "system_temperature_celsius"
        );
        assert_eq!(prom_name("app.req", "req/s").0, "app_req_req_per_s");
        assert_eq!(prom_name("1app", "").0, "_1app");
        assert_eq!(prom_name("kairos.host.up", "1").0, "custom_kairos_host_up");
        assert!(wants_openmetrics(Some(
            "application/openmetrics-text;version=1.0.0,text/plain;q=0.5"
        )));
        assert!(!wants_openmetrics(Some(
            "text/plain;version=0.0.4;q=1,*/*;q=0.1"
        )));
        assert!(!wants_openmetrics(Some("application/openmetrics-text;q=0")));
        assert!(!wants_openmetrics(None));
    }

    #[test]
    fn text_format_is_valid_and_labels_are_complete() {
        let now = 1_760_000_000_000;
        let rows = vec![
            row("a1", true, Some(sample("a1", now - 5_000))),
            row("a2", false, Some(sample("a2", now - 5_000))),
            row("a3", true, Some(sample("a3", now - 600_000))),
            row("a4", true, None),
        ];
        let r = render(&rows, &opts(now));
        let p = kp_obs::prom::parse(&r.body).unwrap_or_else(|e| panic!("{e}\n{}", r.body));
        let h = |a: &'static str| {
            [
                ("agent_id", a),
                ("hostname", "web-01"),
                ("host_group", "B廠"),
            ]
        };
        assert_eq!(p.get("kairos_host_up", &h("a1")), Some(1.0));
        assert_eq!(p.get("kairos_host_up", &h("a2")), Some(0.0));
        assert_eq!(
            p.get("kairos_exporter_hosts", &[("state", "fresh")]),
            Some(1.0)
        );
        assert_eq!(
            p.get("kairos_exporter_hosts", &[("state", "stale")]),
            Some(1.0)
        );
        assert_eq!(
            p.get("kairos_exporter_hosts", &[("state", "offline")]),
            Some(1.0)
        );
        assert_eq!(
            p.get("kairos_exporter_hosts", &[("state", "no_data")]),
            Some(1.0)
        );
        // 只有 a1（線上且未過期）輸出指標
        assert_eq!(p.series("system_cpu_usage_percent").len(), 1);
        assert_eq!(p.get("system_cpu_usage_percent", &h("a1")), Some(12.5));
        let fs: Vec<_> = p.series("system_filesystem_usage_percent");
        assert_eq!(fs[0].labels["mountpoint"], "/");
        assert_eq!(fs[0].labels["fstype"], "ext4");
        // 計數器：_total、原始累計值（u64 不經 f64 失真）
        assert!(r.body.contains("system_network_received_bytes_total{agent_id=\"a1\",hostname=\"web-01\",host_group=\"B廠\",device=\"eth0\"} 18446744073709551000\n"));
        assert_eq!(
            p.families["system_network_received_bytes_total"].0,
            "counter"
        );
        assert_eq!(
            p.get(
                "system_disk_io_time_seconds_total",
                &[
                    ("agent_id", "a1"),
                    ("hostname", "web-01"),
                    ("host_group", "B廠"),
                    ("device", "sda")
                ]
            ),
            Some(12.345)
        );
        assert_eq!(p.families["system_cpu_load_1m"].0, "gauge");
        assert_eq!(p.families["app_queue_depth"].0, "untyped");
        assert_eq!(p.families["system_temperature_k"].0, "untyped");
        let app = p.series("app_queue_depth");
        assert_eq!(app[0].labels["hostname"], "web-01");
        assert_eq!(app[0].labels["exported_hostname"], "x\"y\\z");
        assert!(p.families.contains_key("custom_kairos_host_up"));
        // info 帶顯示名稱、標籤（排序）
        let info = p.series("kairos_host_info");
        assert_eq!(info[0].labels["display_name"], "網頁 01");
        assert_eq!(info[0].labels["tags"], "env:prod,web");
        assert!(
            p.series("kairos_process_cpu_usage_percent").is_empty(),
            "行程預設不輸出"
        );
        assert_eq!(r.hosts, 4);
        assert_eq!(r.series, 11);
    }

    #[test]
    fn openmetrics_and_processes() {
        let now = 1_760_000_000_000;
        let rows = vec![row("a1", true, Some(sample("a1", now)))];
        let mut o = opts(now);
        o.openmetrics = true;
        o.include_processes = true;
        let r = render(&rows, &o);
        assert!(r.body.ends_with("# EOF\n"));
        assert!(
            r.body
                .contains("# TYPE system_network_received_bytes counter\n")
        );
        assert!(r.body.contains("system_network_received_bytes_total{"));
        assert!(r.body.contains("# TYPE app_queue_depth unknown\n"));
        assert!(r.body.contains("kairos_process_cpu_usage_percent{agent_id=\"a1\",hostname=\"web-01\",host_group=\"B廠\",pid=\"42\",process=\"postgres\"} 3.5\n"));
        // OpenMetrics 的樣本部分也能以 text 解析器檢查（去掉 EOF 與以 family 名稱宣告的計數器）
        let text = render(
            &rows,
            &Options {
                openmetrics: false,
                ..o
            },
        );
        kp_obs::prom::parse(&text.body).unwrap();
    }
}
