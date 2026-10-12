//! `reports.body` 的緊湊二進位格式（見 `kairos/STORAGE.md`「report 內容的編碼」）。
//!
//! 每筆 report 一列，指標不重複存名稱、單位與 label：名稱、單位、label 的組合（序列，series）
//! 存在 `series` 表，`body` 中只放序列編號與數值。數值保留收到的 JSON 數字種類
//! （非負整數、負整數、浮點數），解碼後與收到的 `metrics` 相同。
//!
//! ```text
//! format 1（協定 v1 的三欄位）: cpu_usage f32 LE (4 bytes) | varint total_memory | varint used_memory
//! format 2（協定 v2 的通用指標）: varint 個數 | 每個指標：varint series_id | tag (1 byte) | 值
//!   tag 0：非負整數，varint
//!   tag 1：負整數 v，varint(!v as u64)（= -v - 1）
//!   tag 2：浮點數，f64 LE (8 bytes)
//! ```
//!
//! varint 為 LEB128（每 byte 低 7 位元為資料，最高位元表示後面還有）。

use serde_json::Number;

/// `reports.format`：協定 v1 的三欄位（JSONL `schema` 1）
pub const FORMAT_LEGACY: i64 = 1;
/// `reports.format`：協定 v2 的通用指標（JSONL `schema` 2）
pub const FORMAT_METRICS: i64 = 2;

const TAG_UINT: u8 = 0;
const TAG_NEG_INT: u8 = 1;
const TAG_FLOAT: u8 = 2;

/// 一個 MB（v1 的記憶體單位），彙總時換算成 bytes 與 v2 的標準指標合併
pub const BYTES_PER_MB: u64 = 1024 * 1024;

/// 寫入 LEB128 varint
pub fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

/// 讀取 LEB128 varint；資料不足或超過 64 位元時為 `None`
pub fn get_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v: u64 = 0;
    for shift in (0..64).step_by(7) {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        let bits = u64::from(byte & 0x7f);
        if shift == 63 && bits > 1 {
            return None;
        }
        v |= bits << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

fn get_array<const N: usize>(buf: &[u8], pos: &mut usize) -> Option<[u8; N]> {
    let bytes = buf.get(*pos..*pos + N)?;
    *pos += N;
    bytes.try_into().ok()
}

/// 編碼 format 1 的內容
pub fn encode_legacy(buf: &mut Vec<u8>, cpu_usage: f32, total_memory: u64, used_memory: u64) {
    buf.extend_from_slice(&cpu_usage.to_le_bytes());
    put_varint(buf, total_memory);
    put_varint(buf, used_memory);
}

/// 解碼 format 1 的內容：(cpu_usage, total_memory, used_memory)
pub fn decode_legacy(buf: &[u8]) -> Option<(f32, u64, u64)> {
    let mut pos = 0;
    let cpu = f32::from_le_bytes(get_array::<4>(buf, &mut pos)?);
    let total = get_varint(buf, &mut pos)?;
    let used = get_varint(buf, &mut pos)?;
    (pos == buf.len()).then_some((cpu, total, used))
}

/// 編碼一個 JSON 數字（保留整數或浮點數的種類）
fn encode_number(buf: &mut Vec<u8>, value: &Number) {
    if let Some(u) = value.as_u64() {
        buf.push(TAG_UINT);
        put_varint(buf, u);
    } else if let Some(i) = value.as_i64() {
        // as_u64 失敗表示是負數：!i = -i - 1 ≥ 0
        buf.push(TAG_NEG_INT);
        put_varint(buf, !(i as u64));
    } else {
        buf.push(TAG_FLOAT);
        // serde_json 的 Number（未啟用 arbitrary_precision）一定能轉成有限的 f64
        buf.extend_from_slice(&value.as_f64().unwrap_or(0.0).to_le_bytes());
    }
}

fn decode_number(buf: &[u8], pos: &mut usize) -> Option<Number> {
    let tag = *buf.get(*pos)?;
    *pos += 1;
    match tag {
        TAG_UINT => Some(Number::from(get_varint(buf, pos)?)),
        TAG_NEG_INT => {
            let v = !get_varint(buf, pos)? as i64;
            (v < 0).then(|| Number::from(v))
        }
        TAG_FLOAT => Number::from_f64(f64::from_le_bytes(get_array::<8>(buf, pos)?)),
        _ => None,
    }
}

/// 編碼 format 2 的內容；`items` 為 (series_id, 值)
pub fn encode_metrics<'a>(
    buf: &mut Vec<u8>,
    items: impl ExactSizeIterator<Item = (i64, &'a Number)>,
) {
    put_varint(buf, items.len() as u64);
    for (series_id, value) in items {
        put_varint(buf, series_id as u64);
        encode_number(buf, value);
    }
}

/// 解碼 format 2 的內容：[(series_id, 值)]
pub fn decode_metrics(buf: &[u8]) -> Option<Vec<(i64, Number)>> {
    let mut pos = 0;
    let count = get_varint(buf, &mut pos)?;
    // 每個指標至少 3 bytes，避免損毀的個數造成過大的配置
    if count > buf.len() as u64 {
        return None;
    }
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let series_id = i64::try_from(get_varint(buf, &mut pos)?).ok()?;
        out.push((series_id, decode_number(buf, &mut pos)?));
    }
    (pos == buf.len()).then_some(out)
}

/// `series.labels` 的標準形式：沒有 label 時為空字串，否則為依名稱排序的 JSON 物件
/// （例如 `{"fstype":"ext4","mountpoint":"/"}`）
pub fn canonical_labels(labels: &std::collections::BTreeMap<String, String>) -> String {
    if labels.is_empty() {
        String::new()
    } else {
        // BTreeMap 依鍵排序；字串對字串的序列化不會失敗
        serde_json::to_string(labels).unwrap_or_default()
    }
}

/// client 的 `ts`：可解析為 RFC 3339 時存成毫秒（INTEGER），否則原樣存文字（TEXT）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTs {
    Millis(i64),
    Text(String),
}

impl AgentTs {
    /// 只有轉回 RFC 3339 毫秒格式後與原字串相同時才存成毫秒，確保讀回時完全相同
    pub fn parse(ts: &str) -> Self {
        match chrono::DateTime::parse_from_rfc3339(ts) {
            Ok(t) => {
                let t = t.with_timezone(&chrono::Utc);
                if crate::storage::fmt_time(t) == ts {
                    AgentTs::Millis(t.timestamp_millis())
                } else {
                    AgentTs::Text(ts.to_string())
                }
            }
            Err(_) => AgentTs::Text(ts.to_string()),
        }
    }

    /// 還原成收到的字串
    pub fn to_text(&self) -> String {
        match self {
            AgentTs::Millis(ms) => chrono::DateTime::from_timestamp_millis(*ms)
                .map(crate::storage::fmt_time)
                .unwrap_or_default(),
            AgentTs::Text(s) => s.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trip_and_rejects_bad_input() {
        for v in [
            0,
            1,
            127,
            128,
            300,
            16383,
            16384,
            u32::MAX as u64,
            u64::MAX - 1,
            u64::MAX,
        ] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            let mut pos = 0;
            assert_eq!(get_varint(&buf, &mut pos), Some(v), "{v}");
            assert_eq!(pos, buf.len());
        }
        let mut buf = Vec::new();
        put_varint(&mut buf, 1 << 20);
        assert_eq!(buf.len(), 3);
        // 截斷
        assert_eq!(get_varint(&buf[..2], &mut 0), None);
        // 超過 64 位元
        assert_eq!(get_varint(&[0xff; 11], &mut 0), None);
        assert_eq!(
            get_varint(
                &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02],
                &mut 0
            ),
            None
        );
    }

    #[test]
    fn legacy_body_round_trip() {
        let mut buf = Vec::new();
        encode_legacy(&mut buf, 17.84, 49152, 37034);
        assert_eq!(buf.len(), 4 + 3 + 3);
        assert_eq!(decode_legacy(&buf), Some((17.84, 49152, 37034)));
        assert_eq!(decode_legacy(&buf[..5]), None);
        buf.push(0);
        assert_eq!(decode_legacy(&buf), None, "多出的 bytes 視為損毀");
    }

    #[test]
    fn metrics_body_keeps_number_kinds() {
        let values: Vec<Number> = serde_json::from_str(
            "[17.84, 51539607552, 0, -40, -1, 48.5, 18446744073709551615, -9223372036854775808, 1e300]",
        )
        .unwrap();
        let mut buf = Vec::new();
        encode_metrics(
            &mut buf,
            values.iter().enumerate().map(|(i, v)| (i as i64 + 1, v)),
        );
        let decoded = decode_metrics(&buf).unwrap();
        assert_eq!(decoded.len(), values.len());
        for ((i, (id, v)), original) in decoded.iter().enumerate().zip(&values) {
            assert_eq!(*id, i as i64 + 1);
            assert_eq!(v, original);
            // JSON 表示也相同（整數不會變成小數）
            assert_eq!(v.to_string(), original.to_string());
        }
        // 標準指標的一筆（cpu、記憶體 2 個）大約 2 + 10 + 2×7 bytes
        let mut small = Vec::new();
        let three: Vec<Number> = serde_json::from_str("[17.84, 51539607552, 38833356800]").unwrap();
        encode_metrics(&mut small, three.iter().map(|v| (1, v)));
        assert!(small.len() <= 30, "{}", small.len());

        assert_eq!(decode_metrics(&buf[..buf.len() - 1]), None);
        assert_eq!(decode_metrics(&[5, 1]), None);
        assert_eq!(decode_metrics(&[1, 1, 9, 0]), None, "未知的 tag");
        assert_eq!(decode_metrics(&[0]), Some(Vec::new()));
    }

    #[test]
    fn labels_are_canonical() {
        let mut labels = std::collections::BTreeMap::new();
        assert_eq!(canonical_labels(&labels), "");
        labels.insert("mountpoint".to_string(), "/".to_string());
        labels.insert("fstype".to_string(), "ext4".to_string());
        assert_eq!(
            canonical_labels(&labels),
            r#"{"fstype":"ext4","mountpoint":"/"}"#
        );
    }

    #[test]
    fn agent_ts_is_compact_only_when_lossless() {
        let ts = "2026-10-08T03:15:45.210Z";
        let parsed = AgentTs::parse(ts);
        assert!(matches!(parsed, AgentTs::Millis(_)), "{parsed:?}");
        assert_eq!(parsed.to_text(), ts);
        // 其他寫法（時區、精度不同）原樣保存
        for other in [
            "2026-10-08T11:15:45.210+08:00",
            "2026-10-08T03:15:45Z",
            "t",
            "",
        ] {
            let parsed = AgentTs::parse(other);
            assert_eq!(parsed, AgentTs::Text(other.to_string()));
            assert_eq!(parsed.to_text(), other);
        }
    }
}
