//! Prometheus 文字格式（exposition format 0.0.4）。
//!
//! - [`Counter`]、[`Gauge`]、[`Histogram`]：以原子操作更新，熱路徑上只有幾次 `fetch_add`，不取鎖。
//! - [`Exposition`]：輸出 `# HELP`、`# TYPE` 與樣本，保證同一個 family 的樣本連續、每個 family
//!   只宣告一次、標籤值與說明正確跳脫。
//! - [`parse`]：嚴格的解析器（語法、family 的宣告順序、重複的序列、直方圖的累積與 `+Inf`），
//!   兩個產品的測試以它驗證端點的輸出。
//!
//! 標籤基數：呼叫端只能用有限集合的值當標籤（原因、結果、種類），不可用 agent_id、IP 或路徑。

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

/// `/metrics` 回應的 Content-Type
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// 延遲直方圖的桶（秒）：0.5 ms～10 s
pub const LATENCY_BUCKETS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// 只增不減的計數
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub fn inc(&self) {
        self.add(1);
    }

    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// 可增可減的數值
#[derive(Debug, Default)]
pub struct Gauge(AtomicI64);

impl Gauge {
    pub const fn new() -> Self {
        Self(AtomicI64::new(0))
    }

    pub fn set(&self, v: i64) {
        self.0.store(v, Ordering::Relaxed);
    }

    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// 時間的直方圖（秒）。每個桶各自計數（非累積），輸出時才累加；`count` 由各桶相加，
/// 所以輸出中 `+Inf` 桶一定等於 `_count`
#[derive(Debug)]
pub struct Histogram {
    bounds: &'static [f64],
    /// `bounds.len() + 1` 格，最後一格是超過最大上界的
    counts: Box<[AtomicU64]>,
    sum_nanos: AtomicU64,
}

impl Histogram {
    pub fn new(bounds: &'static [f64]) -> Self {
        Self {
            bounds,
            counts: (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            sum_nanos: AtomicU64::new(0),
        }
    }

    /// 延遲直方圖（[`LATENCY_BUCKETS`]）
    pub fn latency() -> Self {
        Self::new(LATENCY_BUCKETS)
    }

    pub fn observe(&self, d: Duration) {
        let secs = d.as_secs_f64();
        let i = self
            .bounds
            .iter()
            .position(|b| secs <= *b)
            .unwrap_or(self.bounds.len());
        self.counts[i].fetch_add(1, Ordering::Relaxed);
        let nanos = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        self.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> HistogramSnapshot {
        let mut cumulative = 0u64;
        let mut buckets = Vec::with_capacity(self.bounds.len());
        for (i, bound) in self.bounds.iter().enumerate() {
            cumulative += self.counts[i].load(Ordering::Relaxed);
            buckets.push((*bound, cumulative));
        }
        cumulative += self.counts[self.bounds.len()].load(Ordering::Relaxed);
        HistogramSnapshot {
            buckets,
            count: cumulative,
            sum: self.sum_nanos.load(Ordering::Relaxed) as f64 / 1e9,
        }
    }
}

/// 直方圖的快照：累積的桶（不含 `+Inf`）、總數與總和（秒）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistogramSnapshot {
    pub buckets: Vec<(f64, u64)>,
    pub count: u64,
    pub sum: f64,
}

impl HistogramSnapshot {
    /// 分位數的上界估計（第一個累積數達到 `q × count` 的桶的上界；落在最後一格時為 `+Inf`）
    pub fn quantile_upper_bound(&self, q: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let target = (q.clamp(0.0, 1.0) * self.count as f64).ceil() as u64;
        Some(
            self.buckets
                .iter()
                .find(|(_, c)| *c >= target.max(1))
                .map_or(f64::INFINITY, |(b, _)| *b),
        )
    }
}

/// metric family 的種類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Counter,
    Gauge,
    Histogram,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        }
    }
}

/// 數值的文字表示：整數不帶小數點；NaN 與無限大依格式規定
pub fn format_value(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_string()
    } else if v.is_infinite() {
        if v > 0.0 { "+Inf" } else { "-Inf" }.to_string()
    } else if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn escape_label(v: &str, out: &mut String) {
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

fn escape_help(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\n', "\\n")
}

/// 名稱是否合法（`[a-zA-Z_:][a-zA-Z0-9_:]*`；標籤名稱不可含 `:`）
fn valid_name(name: &str, colon: bool) -> bool {
    let mut chars = name.chars();
    let ok_first = |c: char| c.is_ascii_alphabetic() || c == '_' || (colon && c == ':');
    match chars.next() {
        Some(c) if ok_first(c) => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || (colon && c == ':'))
}

/// 文字格式的輸出
#[derive(Debug, Default)]
pub struct Exposition {
    out: String,
    declared: HashSet<String>,
}

impl Exposition {
    pub fn new() -> Self {
        Self::default()
    }

    /// 宣告一個 family（`# HELP`、`# TYPE`）。之後的 [`Exposition::sample`] 屬於這個 family，直到宣告下一個。
    ///
    /// # Panics
    /// 名稱不合法或同一個 family 宣告兩次（程式錯誤，測試會發現）
    pub fn family(&mut self, name: &str, kind: Kind, help: &str) -> &mut Self {
        assert!(valid_name(name, true), "metric 名稱不合法：{name}");
        assert!(
            self.declared.insert(name.to_string()),
            "metric family 重複宣告：{name}"
        );
        let _ = writeln!(self.out, "# HELP {name} {}", escape_help(help));
        let _ = writeln!(self.out, "# TYPE {name} {}", kind.as_str());
        self
    }

    /// 一個樣本（屬於最近宣告的 family）
    pub fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: f64) -> &mut Self {
        self.out.push_str(name);
        if !labels.is_empty() {
            self.out.push('{');
            for (i, (k, v)) in labels.iter().enumerate() {
                debug_assert!(valid_name(k, false), "標籤名稱不合法：{k}");
                if i > 0 {
                    self.out.push(',');
                }
                self.out.push_str(k);
                self.out.push_str("=\"");
                escape_label(v, &mut self.out);
                self.out.push('"');
            }
            self.out.push('}');
        }
        self.out.push(' ');
        self.out.push_str(&format_value(value));
        self.out.push('\n');
        self
    }

    /// 單一值的 counter（名稱應以 `_total` 結尾）
    pub fn counter(&mut self, name: &str, help: &str, value: u64) -> &mut Self {
        self.family(name, Kind::Counter, help)
            .sample(name, &[], value as f64)
    }

    /// 單一值的 gauge
    pub fn gauge(&mut self, name: &str, help: &str, value: f64) -> &mut Self {
        self.family(name, Kind::Gauge, help)
            .sample(name, &[], value)
    }

    /// 一個標籤、多個值的 counter
    pub fn counter_by(
        &mut self,
        name: &str,
        help: &str,
        label: &str,
        values: &[(&str, u64)],
    ) -> &mut Self {
        self.family(name, Kind::Counter, help);
        for (v, n) in values {
            self.sample(name, &[(label, v)], *n as f64);
        }
        self
    }

    /// 一個標籤、多個值的 gauge
    pub fn gauge_by(
        &mut self,
        name: &str,
        help: &str,
        label: &str,
        values: &[(&str, f64)],
    ) -> &mut Self {
        self.family(name, Kind::Gauge, help);
        for (v, n) in values {
            self.sample(name, &[(label, v)], *n);
        }
        self
    }

    /// 直方圖（family 與一組樣本）
    pub fn histogram(&mut self, name: &str, help: &str, h: &HistogramSnapshot) -> &mut Self {
        self.family(name, Kind::Histogram, help);
        self.histogram_samples(name, &[], h)
    }

    /// 直方圖的一組樣本（帶標籤時可對同一個 family 呼叫多次）
    pub fn histogram_samples(
        &mut self,
        name: &str,
        labels: &[(&str, &str)],
        h: &HistogramSnapshot,
    ) -> &mut Self {
        let bucket = format!("{name}_bucket");
        for (le, c) in &h.buckets {
            let le = format_value(*le);
            let mut l: Vec<(&str, &str)> = labels.to_vec();
            l.push(("le", &le));
            self.sample(&bucket, &l, *c as f64);
        }
        let mut l: Vec<(&str, &str)> = labels.to_vec();
        l.push(("le", "+Inf"));
        self.sample(&bucket, &l, h.count as f64);
        self.sample(&format!("{name}_sum"), labels, h.sum);
        self.sample(&format!("{name}_count"), labels, h.count as f64)
    }

    pub fn finish(self) -> String {
        self.out
    }
}

// ---------------------------------------------------------------------------------------------
// 解析器（測試與 diag 使用）

/// 解析出的一個樣本
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub value: f64,
}

/// 解析結果
#[derive(Debug, Clone, Default)]
pub struct Parsed {
    /// family 名稱 → (種類, 說明)
    pub families: BTreeMap<String, (String, String)>,
    pub samples: Vec<Sample>,
}

impl Parsed {
    /// 名稱與標籤完全相符的樣本值
    pub fn get(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        self.samples
            .iter()
            .find(|s| {
                s.name == name
                    && s.labels.len() == labels.len()
                    && labels
                        .iter()
                        .all(|(k, v)| s.labels.get(*k).map(String::as_str) == Some(*v))
            })
            .map(|s| s.value)
    }

    /// 某個名稱的所有樣本
    pub fn series(&self, name: &str) -> Vec<&Sample> {
        self.samples.iter().filter(|s| s.name == name).collect()
    }

    /// 樣本所屬的 family 名稱（直方圖的 `_bucket`、`_sum`、`_count` 屬於去掉後綴的 family）
    pub fn family_of(&self, sample: &str) -> Option<&str> {
        if let Some((name, _)) = self.families.get_key_value(sample) {
            return Some(name);
        }
        for suffix in ["_bucket", "_sum", "_count"] {
            if let Some(base) = sample.strip_suffix(suffix)
                && let Some((name, (kind, _))) = self.families.get_key_value(base)
                && kind == "histogram"
            {
                return Some(name);
            }
        }
        None
    }
}

fn parse_value(s: &str) -> Option<f64> {
    match s {
        "NaN" => Some(f64::NAN),
        "+Inf" | "Inf" => Some(f64::INFINITY),
        "-Inf" => Some(f64::NEG_INFINITY),
        _ => s.parse().ok(),
    }
}

/// 解析標籤區塊 `{a="1",b="x\"y"}`（`s` 從 `{` 之後開始），回傳標籤與 `}` 之後的位置
fn parse_labels(s: &str) -> Result<(BTreeMap<String, String>, usize), String> {
    let bytes = s.as_bytes();
    let mut labels = BTreeMap::new();
    let mut i = 0;
    loop {
        if i >= bytes.len() {
            return Err("標籤區塊沒有結束".into());
        }
        if bytes[i] == b'}' {
            return Ok((labels, i + 1));
        }
        let start = i;
        while i < bytes.len() && bytes[i] != b'=' {
            i += 1;
        }
        let name = &s[start..i];
        if !valid_name(name, false) {
            return Err(format!("標籤名稱不合法：{name:?}"));
        }
        if s.get(i..i + 2) != Some("=\"") {
            return Err(format!("標籤 {name} 後應為 =\""));
        }
        i += 2;
        let mut value = String::new();
        loop {
            let c = s[i..].chars().next().ok_or("標籤值沒有結束")?;
            i += c.len_utf8();
            match c {
                '"' => break,
                '\\' => {
                    let e = s[i..].chars().next().ok_or("跳脫字元沒有結束")?;
                    i += e.len_utf8();
                    match e {
                        '\\' => value.push('\\'),
                        '"' => value.push('"'),
                        'n' => value.push('\n'),
                        other => return Err(format!("不合法的跳脫 \\{other}")),
                    }
                }
                '\n' => return Err("標籤值含有換行".into()),
                c => value.push(c),
            }
        }
        if labels.insert(name.to_string(), value).is_some() {
            return Err(format!("標籤 {name} 重複"));
        }
        match bytes.get(i) {
            Some(b',') => i += 1,
            Some(b'}') => {}
            _ => return Err("標籤之間應以逗號分隔".into()),
        }
    }
}

/// 嚴格解析文字格式。檢查：每行的語法、`# TYPE` 在該 family 的樣本之前且只出現一次、
/// family 的樣本連續、相同名稱與標籤的序列不重複、直方圖的桶累積遞增且 `+Inf` 等於 `_count`、
/// 內容以換行結尾
pub fn parse(text: &str) -> Result<Parsed, String> {
    let mut p = Parsed::default();
    if !text.is_empty() && !text.ends_with('\n') {
        return Err("內容必須以換行結尾".into());
    }
    let mut seen_series = HashSet::new();
    let mut finished_families: HashSet<String> = HashSet::new();
    let mut current: Option<String> = None;
    let mut helps = HashSet::new();
    for (no, line) in text.lines().enumerate() {
        let err = |m: String| format!("第 {} 行：{m}：{line}", no + 1);
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            let rest = rest.trim_start();
            let mut parts = rest.splitn(3, ' ');
            let keyword = parts.next().unwrap_or("");
            if keyword != "HELP" && keyword != "TYPE" {
                continue;
            }
            let name = parts.next().ok_or_else(|| err("缺少名稱".into()))?;
            if !valid_name(name, true) {
                return Err(err("名稱不合法".into()));
            }
            let arg = parts.next().unwrap_or("");
            if keyword == "HELP" {
                if !helps.insert(name.to_string()) {
                    return Err(err("HELP 重複".into()));
                }
                p.families
                    .entry(name.to_string())
                    .or_insert_with(|| ("untyped".into(), String::new()))
                    .1 = arg.to_string();
            } else {
                if !matches!(
                    arg,
                    "counter" | "gauge" | "histogram" | "summary" | "untyped"
                ) {
                    return Err(err("TYPE 不合法".into()));
                }
                if p.samples
                    .iter()
                    .any(|s| p.family_of(&s.name) == Some(name) || s.name == name)
                {
                    return Err(err("TYPE 必須在樣本之前".into()));
                }
                let entry = p
                    .families
                    .entry(name.to_string())
                    .or_insert_with(|| ("untyped".into(), String::new()));
                if entry.0 != "untyped" {
                    return Err(err("TYPE 重複".into()));
                }
                entry.0 = arg.to_string();
            }
            continue;
        }
        // 樣本
        let name_end = line.find(['{', ' ']).ok_or_else(|| err("缺少值".into()))?;
        let name = &line[..name_end];
        if !valid_name(name, true) {
            return Err(err("名稱不合法".into()));
        }
        let (labels, rest) = if line[name_end..].starts_with('{') {
            let (labels, used) = parse_labels(&line[name_end + 1..]).map_err(err)?;
            (labels, &line[name_end + 1 + used..])
        } else {
            (BTreeMap::new(), &line[name_end..])
        };
        let rest = rest
            .strip_prefix(' ')
            .ok_or_else(|| err("值之前應有一個空白".into()))?;
        let mut fields = rest.split(' ');
        let value = fields
            .next()
            .and_then(parse_value)
            .ok_or_else(|| err("值不合法".into()))?;
        if let Some(ts) = fields.next()
            && ts.parse::<i64>().is_err()
        {
            return Err(err("時間戳記不合法".into()));
        }
        if fields.next().is_some() {
            return Err(err("多餘的欄位".into()));
        }
        let family = p.family_of(name).unwrap_or(name).to_string();
        if current.as_deref() != Some(family.as_str()) {
            if finished_families.contains(&family) {
                return Err(err(format!("family {family} 的樣本不連續")));
            }
            if let Some(prev) = current.take() {
                finished_families.insert(prev);
            }
            current = Some(family.clone());
        }
        let key = format!("{name}{labels:?}");
        if !seen_series.insert(key) {
            return Err(err("序列重複".into()));
        }
        if p.families
            .get(&family)
            .is_some_and(|(k, _)| k == "histogram")
            && name.ends_with("_bucket")
            && !labels.contains_key("le")
        {
            return Err(err("直方圖的桶缺少 le".into()));
        }
        p.samples.push(Sample {
            name: name.to_string(),
            labels,
            value,
        });
    }
    check_histograms(&p)?;
    Ok(p)
}

fn check_histograms(p: &Parsed) -> Result<(), String> {
    for (name, (kind, _)) in &p.families {
        if kind != "histogram" {
            continue;
        }
        // 依「去掉 le 的標籤」分組
        let mut groups: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
        for s in p.series(&format!("{name}_bucket")) {
            let mut l = s.labels.clone();
            let le = l.remove("le").unwrap_or_default();
            let le = parse_value(&le).ok_or(format!("{name}：le 不合法"))?;
            groups
                .entry(format!("{l:?}"))
                .or_default()
                .push((le, s.value));
        }
        for (labels, mut buckets) in groups {
            buckets.sort_by(|a, b| a.0.total_cmp(&b.0));
            if buckets.windows(2).any(|w| w[1].1 < w[0].1) {
                return Err(format!("{name}{labels}：桶不是累積遞增"));
            }
            let inf = buckets
                .last()
                .filter(|(le, _)| le.is_infinite())
                .ok_or(format!("{name}{labels}：缺少 +Inf 桶"))?;
            let count = p
                .series(&format!("{name}_count"))
                .into_iter()
                .find(|s| format!("{:?}", s.labels) == labels)
                .ok_or(format!("{name}{labels}：缺少 _count"))?;
            if count.value != inf.1 {
                return Err(format!("{name}{labels}：+Inf 桶與 _count 不同"));
            }
            if !p
                .series(&format!("{name}_sum"))
                .iter()
                .any(|s| format!("{:?}", s.labels) == labels)
            {
                return Err(format!("{name}{labels}：缺少 _sum"));
            }
        }
    }
    Ok(())
}

/// 檢查命名慣例：counter 以 `_total` 結尾、名稱都有指定的前綴、每個 family 都有說明
pub fn lint(p: &Parsed, prefix: &str) -> Result<(), String> {
    for (name, (kind, help)) in &p.families {
        if !name.starts_with(prefix) {
            return Err(format!("{name} 沒有前綴 {prefix}"));
        }
        if kind == "counter" && !name.ends_with("_total") {
            return Err(format!("counter {name} 應以 _total 結尾"));
        }
        if help.trim().is_empty() {
            return Err(format!("{name} 沒有說明"));
        }
        if kind == "untyped" {
            return Err(format!("{name} 沒有 TYPE"));
        }
    }
    for s in &p.samples {
        if p.family_of(&s.name).is_none() {
            return Err(format!("樣本 {} 沒有宣告 family", s.name));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_buckets_are_cumulative_and_inf_equals_count() {
        let h = Histogram::latency();
        h.observe(Duration::from_micros(100));
        h.observe(Duration::from_millis(3));
        h.observe(Duration::from_millis(3));
        h.observe(Duration::from_secs(60));
        let s = h.snapshot();
        assert_eq!(s.count, 4);
        assert_eq!(s.buckets[0], (0.0005, 1));
        assert_eq!(s.buckets.iter().find(|(b, _)| *b == 0.005).unwrap().1, 3);
        assert_eq!(s.buckets.last().unwrap().1, 3);
        assert!((s.sum - 60.0061).abs() < 1e-6);
        assert_eq!(s.quantile_upper_bound(0.5), Some(0.005));
        assert_eq!(s.quantile_upper_bound(0.99), Some(f64::INFINITY));
        assert_eq!(HistogramSnapshot::default().quantile_upper_bound(0.5), None);
    }

    #[test]
    fn exposition_round_trips_through_the_parser() {
        let h = Histogram::latency();
        h.observe(Duration::from_millis(2));
        let mut e = Exposition::new();
        e.counter("x_requests_total", "請求數\n第二行 \\ 反斜線", 3)
            .gauge("x_ratio", "比例", 0.25)
            .counter_by(
                "x_rejected_total",
                "被拒",
                "reason",
                &[("per_ip", 2), ("quote\"and\\slash\nline", 1)],
            )
            .histogram("x_latency_seconds", "延遲", &h.snapshot());
        e.family("x_info", Kind::Gauge, "版本").sample(
            "x_info",
            &[("version", "0.6.0"), ("commit", "abc")],
            1.0,
        );
        let text = e.finish();
        let p = parse(&text).unwrap();
        lint(&p, "x_").unwrap();
        assert_eq!(p.get("x_requests_total", &[]), Some(3.0));
        assert_eq!(p.get("x_ratio", &[]), Some(0.25));
        assert_eq!(
            p.get("x_rejected_total", &[("reason", "quote\"and\\slash\nline")]),
            Some(1.0)
        );
        assert_eq!(
            p.get("x_latency_seconds_bucket", &[("le", "+Inf")]),
            Some(1.0)
        );
        assert_eq!(p.get("x_latency_seconds_count", &[]), Some(1.0));
        assert_eq!(
            p.families["x_requests_total"].1,
            "請求數\\n第二行 \\\\ 反斜線"
        );
    }

    #[test]
    fn parser_rejects_malformed_text() {
        let bad = [
            "x 1",                                                      // 沒有換行結尾
            "x{a=\"1\" 1\n",                                            // 標籤沒有結束
            "x{a=1} 1\n",                                               // 標籤值沒有引號
            "x{1a=\"1\"} 1\n",                                          // 標籤名稱不合法
            "x{a=\"1\",a=\"2\"} 1\n",                                   // 標籤重複
            "x one\n",                                                  // 值不合法
            "x 1\nx 2\n",                                               // 序列重複
            "# TYPE x counter\n# TYPE x counter\nx 1\n",                // TYPE 重複
            "x 1\n# TYPE x counter\n",                                  // TYPE 在樣本之後
            "# TYPE x gauge\nx 1\n# TYPE y gauge\ny 1\nx{a=\"b\"} 2\n", // 不連續
            "# TYPE h histogram\nh_bucket{le=\"1\"} 2\nh_bucket{le=\"+Inf\"} 1\nh_sum 1\nh_count 1\n",
            "# TYPE h histogram\nh_bucket{le=\"1\"} 1\nh_sum 1\nh_count 1\n", // 沒有 +Inf
            "# TYPE h histogram\nh_bucket{le=\"+Inf\"} 1\nh_sum 1\nh_count 2\n",
            "# TYPE x bogus\n",
            "9x 1\n",
        ];
        for b in bad {
            assert!(parse(b).is_err(), "應被拒：{b:?}");
        }
        assert!(parse("").unwrap().samples.is_empty());
        assert!(parse("# 一般註解\nx 1 1700000000000\n").is_ok());
    }

    #[test]
    fn lint_enforces_naming() {
        let p = parse("# HELP a_x 說明\n# TYPE a_x counter\na_x 1\n").unwrap();
        assert!(lint(&p, "a_").unwrap_err().contains("_total"));
        let p = parse("# TYPE a_x_total counter\na_x_total 1\n").unwrap();
        assert!(lint(&p, "a_").unwrap_err().contains("說明"));
        let p = parse("# HELP b_total x\n# TYPE b_total counter\nb_total 1\n").unwrap();
        assert!(lint(&p, "a_").unwrap_err().contains("前綴"));
    }

    #[test]
    fn value_formatting() {
        assert_eq!(format_value(3.0), "3");
        assert_eq!(format_value(0.25), "0.25");
        assert_eq!(format_value(f64::INFINITY), "+Inf");
        assert_eq!(format_value(f64::NEG_INFINITY), "-Inf");
        assert_eq!(format_value(f64::NAN), "NaN");
        assert_eq!(format_value(-2.0), "-2");
    }

    #[test]
    fn gauge_and_counter() {
        let g = Gauge::new();
        g.inc();
        g.inc();
        g.dec();
        assert_eq!(g.get(), 1);
        g.set(-5);
        assert_eq!(g.get(), -5);
        let c = Counter::new();
        c.add(5);
        c.inc();
        assert_eq!(c.get(), 6);
    }
}
