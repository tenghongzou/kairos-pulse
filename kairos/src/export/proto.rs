//! 最小的 protobuf 編碼器（OTLP 的 `ExportMetricsServiceRequest` 只需要 varint、fixed64 與
//! length-delimited 三種 wire type）。不引入 prost／opentelemetry-proto：OTLP 指標只用到十來個訊息，
//! 手寫編碼不到兩百行，避免 build script（protoc）與數十個傳遞依賴（`kairos/EXPORT.md`「依賴」）。
//!
//! 欄位編號依 opentelemetry-proto v1.x（`opentelemetry/proto/{collector/metrics/v1,metrics/v1,
//! common/v1,resource/v1}/*.proto`），之後的版本只新增欄位，不改既有的編號。

/// wire type
const VARINT: u8 = 0;
const FIXED64: u8 = 1;
const LEN: u8 = 2;

/// 一個訊息的編碼結果
#[derive(Debug, Default, Clone)]
pub struct Msg(pub Vec<u8>);

impl Msg {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    fn varint(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.0.push(b);
                return;
            }
            self.0.push(b | 0x80);
        }
    }

    fn key(&mut self, field: u32, wire: u8) {
        self.varint((u64::from(field) << 3) | u64::from(wire));
    }

    /// varint 欄位（int64、uint64、enum、bool）；0 不輸出（proto3 的預設值）
    pub fn uint(&mut self, field: u32, v: u64) -> &mut Self {
        if v != 0 {
            self.key(field, VARINT);
            self.varint(v);
        }
        self
    }

    /// int64（負數以 10 bytes 的二補數 varint 表示）
    #[cfg(test)]
    pub fn int(&mut self, field: u32, v: i64) -> &mut Self {
        self.uint(field, v as u64)
    }

    pub fn boolean(&mut self, field: u32, v: bool) -> &mut Self {
        self.uint(field, u64::from(v))
    }

    /// fixed64（`time_unix_nano` 等）；0 不輸出
    pub fn fixed64(&mut self, field: u32, v: u64) -> &mut Self {
        if v != 0 {
            self.key(field, FIXED64);
            self.0.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    /// sfixed64（`NumberDataPoint.as_int`）：oneof 的成員，0 也要輸出
    pub fn sfixed64_always(&mut self, field: u32, v: i64) -> &mut Self {
        self.key(field, FIXED64);
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// double（`NumberDataPoint.as_double`）：oneof 的成員，0 也要輸出
    pub fn double_always(&mut self, field: u32, v: f64) -> &mut Self {
        self.key(field, FIXED64);
        self.0.extend_from_slice(&v.to_bits().to_le_bytes());
        self
    }

    pub fn bytes(&mut self, field: u32, v: &[u8]) -> &mut Self {
        self.key(field, LEN);
        self.varint(v.len() as u64);
        self.0.extend_from_slice(v);
        self
    }

    /// string；空字串不輸出
    pub fn string(&mut self, field: u32, v: &str) -> &mut Self {
        if !v.is_empty() {
            self.bytes(field, v.as_bytes());
        }
        self
    }

    /// string；oneof 的成員（`AnyValue.string_value`），空字串也要輸出
    pub fn string_always(&mut self, field: u32, v: &str) -> &mut Self {
        self.bytes(field, v.as_bytes())
    }

    /// 內嵌訊息（空的也輸出，表示「有這個欄位」）
    pub fn message(&mut self, field: u32, m: &Msg) -> &mut Self {
        self.bytes(field, &m.0)
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

// ---- 解碼（測試、假的 collector、collector 的回應 `partial_success`） ----

/// 一個欄位的值
#[derive(Debug, Clone, PartialEq)]
pub enum Value<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

impl<'a> Value<'a> {
    pub fn bytes(&self) -> &'a [u8] {
        match self {
            Value::Bytes(b) => b,
            _ => &[],
        }
    }

    pub fn str(&self) -> &'a str {
        std::str::from_utf8(self.bytes()).unwrap_or("")
    }

    pub fn u64(&self) -> u64 {
        match self {
            Value::Varint(v) | Value::Fixed64(v) => *v,
            Value::Fixed32(v) => u64::from(*v),
            Value::Bytes(_) => 0,
        }
    }

    #[cfg(test)]
    pub fn f64(&self) -> f64 {
        f64::from_bits(self.u64())
    }
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64, String> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *buf.get(*pos).ok_or("varint 被截斷")?;
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err("varint 太長".into())
}

/// 解出一個訊息的所有欄位（依出現順序）
pub fn decode(buf: &[u8]) -> Result<Vec<(u32, Value<'_>)>, String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        let key = read_varint(buf, &mut pos)?;
        let field = u32::try_from(key >> 3).map_err(|_| "欄位編號太大")?;
        let v = match (key & 7) as u8 {
            VARINT => Value::Varint(read_varint(buf, &mut pos)?),
            FIXED64 => {
                let b = buf.get(pos..pos + 8).ok_or("fixed64 被截斷")?;
                pos += 8;
                Value::Fixed64(u64::from_le_bytes(b.try_into().unwrap_or([0; 8])))
            }
            LEN => {
                let n = usize::try_from(read_varint(buf, &mut pos)?).map_err(|_| "長度太大")?;
                let b = buf.get(pos..pos + n).ok_or("length-delimited 被截斷")?;
                pos += n;
                Value::Bytes(b)
            }
            5 => {
                let b = buf.get(pos..pos + 4).ok_or("fixed32 被截斷")?;
                pos += 4;
                Value::Fixed32(u32::from_le_bytes(b.try_into().unwrap_or([0; 4])))
            }
            w => return Err(format!("不支援的 wire type {w}")),
        };
        out.push((field, v));
    }
    Ok(out)
}

/// 第一個編號為 `field` 的欄位
pub fn field<'a>(fields: &[(u32, Value<'a>)], f: u32) -> Option<Value<'a>> {
    fields.iter().find(|(n, _)| *n == f).map(|(_, v)| v.clone())
}

/// 所有編號為 `field` 的欄位（repeated）
#[cfg(test)]
pub fn fields<'a>(fields: &[(u32, Value<'a>)], f: u32) -> Vec<Value<'a>> {
    fields
        .iter()
        .filter(|(n, _)| *n == f)
        .map(|(_, v)| v.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_of_every_wire_type() {
        let mut inner = Msg::new();
        inner.string(1, "host.name").uint(3, 300);
        let mut m = Msg::new();
        m.message(1, &inner)
            .fixed64(3, 1_700_000_000_000_000_000)
            .double_always(4, 0.0)
            .sfixed64_always(6, -5)
            .int(2, -1)
            .boolean(9, true)
            .string(10, "")
            .uint(11, 0);
        let bytes = m.into_bytes();
        let f = decode(&bytes).unwrap();
        // 0 與空字串不輸出；oneof 的 0 要輸出
        assert_eq!(
            f.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            [1, 3, 4, 6, 2, 9]
        );
        let i = decode(field(&f, 1).unwrap().bytes()).unwrap();
        assert_eq!(field(&i, 1).unwrap().str(), "host.name");
        assert_eq!(field(&i, 3).unwrap().u64(), 300);
        assert_eq!(field(&f, 3).unwrap().u64(), 1_700_000_000_000_000_000);
        assert_eq!(field(&f, 4).unwrap().f64(), 0.0);
        assert_eq!(field(&f, 6).unwrap().u64() as i64, -5);
        assert_eq!(field(&f, 2).unwrap().u64() as i64, -1);
        assert!(decode(&[0x0a, 0x05, 1]).is_err(), "截斷的長度");
    }
}
