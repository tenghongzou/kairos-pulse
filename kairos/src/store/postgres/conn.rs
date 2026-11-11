//! PostgreSQL 後端的連線（[`PgConn`]）與交易（[`PgTx`]）：實作所有領域 trait 的 [`Store`]／[`Tx`]。
//!
//! 介面刻意與 rusqlite 相同（`execute`、`query_row`、`prepare` → `query_map`、`.optional()`、`params!`），
//! 讓 `store/postgres/<領域>.rs` 可以逐函式對照 `store/sqlite/<領域>.rs`：兩邊的差別只剩 SQL 方言
//! （`kairos/HA.md` 第 5.3 節）與並行控制（第 5.4 節）。
//!
//! - **佔位符**：SQL 以 `?1`、`?2`（或依序的 `?`）撰寫，送出前機械式地換成 `$1`、`$2`
//!   （[`translate_placeholders`]；字串常值與註解內的 `?` 不動）。敘述以原文為鍵快取在連線上。
//! - **參數**（[`Value`]）：依伺服器推斷出的型別編碼（`BIGINT`、`INTEGER`、`BOOLEAN`、`TEXT`、`BYTEA`、
//!   `DOUBLE PRECISION` 與它們的陣列），所以 Rust 端一律以 `i64`／`f64`／字串／位元組傳入即可。
//! - **讀取**（[`Row::get`]）：依欄位的實際型別解碼再轉成要求的 Rust 型別（整數之間、`BOOLEAN`↔整數、
//!   `NUMERIC`→整數或浮點數；`meta.value` 這類以文字保存的數值也能讀成整數）。
//! - **交易**：[`TxMode::Immediate`] 在 `BEGIN` 之後取得交易層級的 advisory lock，把所有「序列化的寫入
//!   交易」排成一列，語意與 SQLite 的 `BEGIN IMMEDIATE`（全域寫入鎖）相同，跨節點也成立。report 的寫入
//!   路徑（[`super::report`]）不經過這個鎖。

use postgres::fallible_iterator::FallibleIterator;
use postgres::types::private::BytesMut;
use postgres::types::{FromSql, IsNull, Kind, ToSql, Type, to_sql_checked};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use super::pool::PgPool;
use crate::store::db::{Store, StoreError, StoreErrorKind, StoreResult, Tx, TxMode};

/// 執行會阻塞的資料庫操作。同步的 `postgres` 驅動內部以自己的 runtime `block_on`，不能直接在 tokio 的
/// worker 執行緒上呼叫（會 panic）；儲存層的呼叫端都在 blocking 執行緒上（`spawn_blocking`）或在 runtime
/// 之外（CLI），這裡是保險：在多執行緒 runtime 的 worker 上時以 `block_in_place` 讓出 worker
pub(super) fn blocking<R>(f: impl FnOnce() -> R) -> R {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// 快取的預備敘述上限（動態組出的 SQL 很多時整批清掉重來）
const STATEMENT_CACHE_LIMIT: usize = 512;

/// advisory lock 的種類（鍵的低 32 位元再與 schema 名稱的雜湊混合，見 [`PgPool::lock_key`]）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockKind {
    /// 序列化的寫入交易（[`TxMode::Immediate`]）
    Write = 1,
    /// schema 遷移（`kairos/HA.md` 第 5.5 節）
    Migrate = 2,
    /// 稽核雜湊鏈的寫入（`kairos/HA.md` 第 12.1 節：所有寫入者序列化，鏈的順序等於 commit 的順序）
    Audit = 3,
    /// 「服務執行中」：Kairos 的 writer 連線以 session 層級的共享鎖持有；需要獨占資料庫的 CLI
    /// （離線刪除主機資料、`kairos db migrate`）取得互斥鎖，取不到表示有 Kairos 正在使用這個資料庫
    /// （對應 SQLite 資料庫檔的 EXCLUSIVE 鎖）
    Service = 4,
    /// 「這個 schema 由誰服務」（啟動保護）：沒有啟用叢集模式的 Kairos 以 session 層級的**互斥**鎖持有，
    /// 叢集模式的節點（含備援）以**共享**鎖持有。因此同一個 schema 上，單機模式的 Kairos 同時只能有一個，
    /// 也不能與叢集的節點並存；叢集的節點彼此可以並存（由 leader 租約決定誰服務）
    Mode = 5,
}

/// PostgreSQL 後端的錯誤
pub enum PgError {
    /// 查詢應回傳一列但沒有（rusqlite 的 `QueryReturnedNoRows`）
    NoRows,
    /// 驅動或伺服器的錯誤
    Db(postgres::Error),
    /// 型別轉換、用法錯誤等
    Other(String),
    /// 呼叫端的 I/O 錯誤（匯出時輸出端回報的錯誤；保留種類，例如 `BrokenPipe`）
    Io(std::io::Error),
}

pub type PgResult<T> = Result<T, PgError>;

impl PgError {
    pub fn other(msg: impl Into<String>) -> Self {
        PgError::Other(msg.into())
    }

    /// SQLSTATE（伺服器回報的錯誤才有）
    pub fn sqlstate(&self) -> Option<&str> {
        match self {
            PgError::Db(e) => e.code().map(|c| c.code()),
            _ => None,
        }
    }

    /// 違反唯一鍵（`23505`）
    pub fn is_unique_violation(&self) -> bool {
        self.sqlstate() == Some("23505")
    }

    /// 可以重試整個交易的錯誤：序列化失敗（`40001`）、死結（`40P01`）
    pub fn is_retryable(&self) -> bool {
        matches!(self.sqlstate(), Some("40001" | "40P01"))
    }

    /// 連線已經不能用（中斷、協定錯誤）：結果不明，呼叫端應該丟棄這條連線
    pub fn is_connection_fatal(&self) -> bool {
        match self {
            PgError::Db(e) => e.is_closed() || e.code().is_none(),
            _ => false,
        }
    }

    pub fn kind(&self) -> StoreErrorKind {
        match self {
            PgError::NoRows => StoreErrorKind::NotFound,
            PgError::Other(_) | PgError::Io(_) => StoreErrorKind::Other,
            PgError::Db(_) => match self.sqlstate() {
                Some(c) if c.starts_with("23") => StoreErrorKind::Constraint,
                // 鎖等待逾時、敘述逾時、序列化失敗、死結：暫時性
                Some("55P03" | "57014" | "40001" | "40P01") => StoreErrorKind::Busy,
                Some("53100") => StoreErrorKind::Full,
                _ => StoreErrorKind::Other,
            },
        }
    }
}

impl fmt::Display for PgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PgError::NoRows => f.write_str("Query returned no rows"),
            PgError::Other(m) => f.write_str(m),
            PgError::Io(e) => write!(f, "{e}"),
            PgError::Db(e) => match e.as_db_error() {
                Some(d) => write!(
                    f,
                    "{}: {}（SQLSTATE {}）",
                    d.severity(),
                    d.message(),
                    d.code().code()
                ),
                None => {
                    // postgres::Error 的 Display 只有種類（「error communicating with the server」），原因在 source
                    write!(f, "{e}")?;
                    if let Some(src) = std::error::Error::source(e) {
                        write!(f, ": {src}")?;
                    }
                    Ok(())
                }
            },
        }
    }
}

impl fmt::Debug for PgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for PgError {}

impl From<postgres::Error> for PgError {
    fn from(e: postgres::Error) -> Self {
        PgError::Db(e)
    }
}

impl From<PgError> for StoreError {
    fn from(e: PgError) -> Self {
        match e {
            // 與 SQLite 後端相同：I/O 錯誤原樣帶出（`StoreError::io_kind`）
            PgError::Io(io) => StoreError::from(io),
            e => StoreError::new(e.kind(), e),
        }
    }
}

impl From<PgError> for std::io::Error {
    fn from(e: PgError) -> Self {
        match e {
            PgError::Io(io) => io,
            e => std::io::Error::other(e.to_string()),
        }
    }
}

/// `.optional()`：把「沒有列」變成 `None`（與 rusqlite 的 `OptionalExtension` 相同）
pub trait OptionalExtension<T> {
    fn optional(self) -> PgResult<Option<T>>;
}

impl<T> OptionalExtension<T> for PgResult<T> {
    fn optional(self) -> PgResult<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(PgError::NoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 參數
// ---------------------------------------------------------------------------------------------

/// 一個參數或欄位的值（與 rusqlite 的 `types::Value` 相同的五種，另加陣列）
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
    /// 陣列參數（`= ANY(?1)`、`unnest(?1::bigint[])`）；SQL 中必須明確轉型成陣列型別
    Array(Vec<Value>),
}

type BoxError = Box<dyn std::error::Error + Sync + Send>;

fn is_text_type(ty: &Type) -> bool {
    matches!(
        *ty,
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN
    )
}

impl ToSql for Value {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        match self {
            Value::Null => Ok(IsNull::Yes),
            Value::Integer(v) => match *ty {
                Type::INT8 => v.to_sql(ty, out),
                Type::INT4 => i32::try_from(*v)
                    .map_err(|_| format!("整數 {v} 超出 INTEGER 的範圍"))?
                    .to_sql(ty, out),
                Type::INT2 => i16::try_from(*v)
                    .map_err(|_| format!("整數 {v} 超出 SMALLINT 的範圍"))?
                    .to_sql(ty, out),
                Type::BOOL => (*v != 0).to_sql(ty, out),
                Type::FLOAT8 => (*v as f64).to_sql(ty, out),
                Type::FLOAT4 => (*v as f32).to_sql(ty, out),
                Type::OID => u32::try_from(*v)
                    .map_err(|_| format!("整數 {v} 超出 OID 的範圍"))?
                    .to_sql(ty, out),
                _ if is_text_type(ty) => v.to_string().as_str().to_sql(&Type::TEXT, out),
                _ => Err(format!("整數參數無法轉成 PostgreSQL 型別 {ty}").into()),
            },
            Value::Real(v) => match *ty {
                Type::FLOAT8 => v.to_sql(ty, out),
                Type::FLOAT4 => (*v as f32).to_sql(ty, out),
                _ if is_text_type(ty) => v.to_string().as_str().to_sql(&Type::TEXT, out),
                _ => Err(format!("浮點數參數無法轉成 PostgreSQL 型別 {ty}").into()),
            },
            Value::Text(s) => match *ty {
                _ if is_text_type(ty) => s.as_str().to_sql(&Type::TEXT, out),
                Type::JSON => s.as_str().to_sql(&Type::TEXT, out),
                Type::JSONB => {
                    // jsonb 的二進位格式：版本位元組 1 加上文字
                    out.extend_from_slice(&[1]);
                    out.extend_from_slice(s.as_bytes());
                    Ok(IsNull::No)
                }
                Type::INT8 => s
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| format!("文字參數「{s}」無法轉成 BIGINT"))?
                    .to_sql(ty, out),
                Type::BYTEA => s.as_bytes().to_sql(ty, out),
                _ => Err(format!("文字參數無法轉成 PostgreSQL 型別 {ty}").into()),
            },
            Value::Blob(b) => match *ty {
                Type::BYTEA => b.as_slice().to_sql(ty, out),
                _ => Err(format!("位元組參數無法轉成 PostgreSQL 型別 {ty}").into()),
            },
            Value::Array(items) => match ty.kind() {
                Kind::Array(_) => items.to_sql(ty, out),
                _ => Err(format!(
                    "陣列參數無法轉成 PostgreSQL 型別 {ty}（SQL 中需要明確轉型，例如 ?1::bigint[]）"
                )
                .into()),
            },
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

/// 可以當參數的 Rust 型別
pub trait ToValue {
    fn to_value(&self) -> Value;
}

macro_rules! int_to_value {
    ($($t:ty),*) => {$(
        impl ToValue for $t {
            fn to_value(&self) -> Value {
                // u64／usize 超過 i64 的範圍時飽和（資料庫中沒有這麼大的值；與 rusqlite 回錯誤不同，
                // 但這些值都來自長度與計數）
                Value::Integer(i64::try_from(*self).unwrap_or(i64::MAX))
            }
        }
    )*};
}
int_to_value!(i64, i32, i16, i8, u8, u16, u32, u64, usize);

impl ToValue for bool {
    fn to_value(&self) -> Value {
        Value::Integer(i64::from(*self))
    }
}
impl ToValue for f64 {
    fn to_value(&self) -> Value {
        Value::Real(*self)
    }
}
impl ToValue for f32 {
    fn to_value(&self) -> Value {
        Value::Real(f64::from(*self))
    }
}
impl ToValue for str {
    fn to_value(&self) -> Value {
        Value::Text(self.to_string())
    }
}
impl ToValue for String {
    fn to_value(&self) -> Value {
        Value::Text(self.clone())
    }
}
impl ToValue for [u8] {
    fn to_value(&self) -> Value {
        Value::Blob(self.to_vec())
    }
}
impl ToValue for Vec<u8> {
    fn to_value(&self) -> Value {
        Value::Blob(self.clone())
    }
}
impl<const N: usize> ToValue for [u8; N] {
    fn to_value(&self) -> Value {
        Value::Blob(self.to_vec())
    }
}
impl ToValue for Value {
    fn to_value(&self) -> Value {
        self.clone()
    }
}
impl<T: ToValue> ToValue for Option<T> {
    fn to_value(&self) -> Value {
        match self {
            Some(v) => v.to_value(),
            None => Value::Null,
        }
    }
}
impl<T: ToValue + ?Sized> ToValue for &T {
    fn to_value(&self) -> Value {
        (**self).to_value()
    }
}
impl<T: ToValue + ?Sized> ToValue for Box<T> {
    fn to_value(&self) -> Value {
        (**self).to_value()
    }
}
impl ToValue for std::borrow::Cow<'_, str> {
    fn to_value(&self) -> Value {
        Value::Text(self.to_string())
    }
}

/// 陣列參數：`array(&ids)`（SQL 中寫 `= ANY(?1::bigint[])` 或 `unnest(?1::text[])`）
pub fn array<T: ToValue>(items: impl IntoIterator<Item = T>) -> Value {
    Value::Array(items.into_iter().map(|v| v.to_value()).collect())
}

/// 一個敘述的參數（與 rusqlite 的 `Params` 對應：`params![…]`、`[a, b]`、`()`、`(a,)`、`(a, b)`…）
pub trait Params {
    fn into_values(self) -> Vec<Value>;
}

impl Params for Vec<Value> {
    fn into_values(self) -> Vec<Value> {
        self
    }
}
impl Params for &[Value] {
    fn into_values(self) -> Vec<Value> {
        self.to_vec()
    }
}
impl Params for &[&dyn ToValue] {
    fn into_values(self) -> Vec<Value> {
        self.iter().map(|v| v.to_value()).collect()
    }
}
impl Params for () {
    fn into_values(self) -> Vec<Value> {
        Vec::new()
    }
}
impl<T: ToValue, const N: usize> Params for [T; N] {
    fn into_values(self) -> Vec<Value> {
        self.iter().map(ToValue::to_value).collect()
    }
}
impl<T: ToValue, const N: usize> Params for &[T; N] {
    fn into_values(self) -> Vec<Value> {
        self.iter().map(ToValue::to_value).collect()
    }
}
macro_rules! tuple_params {
    ($(($($n:ident),+)),*) => {$(
        impl<$($n: ToValue),+> Params for ($($n,)+) {
            #[allow(non_snake_case)]
            fn into_values(self) -> Vec<Value> {
                let ($($n,)+) = self;
                vec![$($n.to_value()),+]
            }
        }
    )*};
}
tuple_params!(
    (A),
    (A, B),
    (A, B, C),
    (A, B, C, D),
    (A, B, C, D, E),
    (A, B, C, D, E, F)
);

/// `params_from_iter(iter)`：動態個數的參數
pub fn params_from_iter<T: ToValue>(iter: impl IntoIterator<Item = T>) -> Vec<Value> {
    iter.into_iter().map(|v| v.to_value()).collect()
}

/// `params![a, b, c]`：與 rusqlite 的同名巨集相同的寫法
macro_rules! params {
    () => {
        ::std::vec::Vec::<$crate::store::postgres::conn::Value>::new()
    };
    ($($p:expr),+ $(,)?) => {
        vec![$($crate::store::postgres::conn::ToValue::to_value(&$p)),+]
    };
}
pub(crate) use params;

// ---------------------------------------------------------------------------------------------
// 讀取
// ---------------------------------------------------------------------------------------------

/// 一個欄位的原始內容（型別與二進位格式的位元組；`NULL` 時為 `None`）
struct RawCell<'a> {
    ty: Type,
    raw: Option<&'a [u8]>,
}

impl<'a> FromSql<'a> for RawCell<'a> {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        Ok(RawCell {
            ty: ty.clone(),
            raw: Some(raw),
        })
    }

    fn from_sql_null(ty: &Type) -> Result<Self, BoxError> {
        Ok(RawCell {
            ty: ty.clone(),
            raw: None,
        })
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

/// `NUMERIC` 的二進位格式 → f64（`sum(bigint)`、`avg()` 的結果；Kairos 的資料表沒有 `NUMERIC` 欄位）
fn numeric_to_f64(raw: &[u8]) -> Result<f64, String> {
    if raw.len() < 8 {
        return Err("NUMERIC 的內容太短".to_string());
    }
    let word = |i: usize| i16::from_be_bytes([raw[i * 2], raw[i * 2 + 1]]);
    let ndigits = usize::try_from(word(0)).map_err(|_| "NUMERIC 的位數不合法".to_string())?;
    let weight = i32::from(word(1));
    let sign = u16::from_be_bytes([raw[4], raw[5]]);
    if raw.len() < 8 + ndigits * 2 {
        return Err("NUMERIC 的內容不完整".to_string());
    }
    if sign == 0xC000 {
        return Ok(f64::NAN);
    }
    let mut v = 0.0_f64;
    for i in 0..ndigits {
        let digit = f64::from(word(4 + i));
        v += digit * 10000_f64.powi(weight - i32::try_from(i).unwrap_or(i32::MAX));
    }
    Ok(if sign == 0x4000 { -v } else { v })
}

fn fixed<const N: usize>(raw: &[u8], what: &str) -> Result<[u8; N], String> {
    <[u8; N]>::try_from(raw).map_err(|_| format!("{what} 欄位的長度不是 {N} 位元組"))
}

impl RawCell<'_> {
    fn to_value(&self) -> Result<Value, String> {
        let Some(raw) = self.raw else {
            return Ok(Value::Null);
        };
        Ok(match self.ty {
            Type::INT8 => Value::Integer(i64::from_be_bytes(fixed(raw, "BIGINT")?)),
            Type::INT4 => Value::Integer(i64::from(i32::from_be_bytes(fixed(raw, "INTEGER")?))),
            Type::INT2 => Value::Integer(i64::from(i16::from_be_bytes(fixed(raw, "SMALLINT")?))),
            Type::OID => Value::Integer(i64::from(u32::from_be_bytes(fixed(raw, "OID")?))),
            Type::BOOL => Value::Integer(i64::from(raw.first().is_some_and(|b| *b != 0))),
            Type::FLOAT8 => Value::Real(f64::from_be_bytes(fixed(raw, "DOUBLE PRECISION")?)),
            Type::FLOAT4 => Value::Real(f64::from(f32::from_be_bytes(fixed(raw, "REAL")?))),
            Type::NUMERIC => {
                let v = numeric_to_f64(raw)?;
                // 整數值（count、sum 的結果）以整數回傳；2^53 以內精確
                if v.fract() == 0.0 && v.abs() < 9_007_199_254_740_992.0 {
                    #[allow(clippy::cast_possible_truncation)]
                    Value::Integer(v as i64)
                } else {
                    Value::Real(v)
                }
            }
            Type::BYTEA => Value::Blob(raw.to_vec()),
            Type::JSONB => {
                let text = raw.get(1..).unwrap_or_default();
                Value::Text(
                    std::str::from_utf8(text)
                        .map_err(|e| format!("JSONB 欄位不是 UTF-8: {e}"))?
                        .to_string(),
                )
            }
            _ if is_text_type(&self.ty) || self.ty == Type::JSON => Value::Text(
                std::str::from_utf8(raw)
                    .map_err(|e| format!("文字欄位不是 UTF-8: {e}"))?
                    .to_string(),
            ),
            _ => return Err(format!("不支援的 PostgreSQL 欄位型別 {}", self.ty)),
        })
    }
}

/// 可以從欄位讀出的 Rust 型別
pub trait FromValue: Sized {
    fn from_value(v: Value) -> Result<Self, String>;
}

fn kind_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NULL",
        Value::Integer(_) => "整數",
        Value::Real(_) => "浮點數",
        Value::Text(_) => "文字",
        Value::Blob(_) => "位元組",
        Value::Array(_) => "陣列",
    }
}

fn int_of(v: Value) -> Result<i64, String> {
    match v {
        Value::Integer(i) => Ok(i),
        // 以文字保存的數值（`meta.value`）
        Value::Text(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("文字「{s}」不是整數")),
        other => Err(format!("欄位是{}，不是整數", kind_name(&other))),
    }
}

macro_rules! int_from_value {
    ($($t:ty),*) => {$(
        impl FromValue for $t {
            fn from_value(v: Value) -> Result<Self, String> {
                let i = int_of(v)?;
                <$t>::try_from(i).map_err(|_| format!("整數 {i} 超出 {} 的範圍", stringify!($t)))
            }
        }
    )*};
}
int_from_value!(i64, i32, i16, i8, u8, u16, u32, u64, usize);

impl FromValue for bool {
    fn from_value(v: Value) -> Result<Self, String> {
        Ok(int_of(v)? != 0)
    }
}
impl FromValue for f64 {
    fn from_value(v: Value) -> Result<Self, String> {
        match v {
            Value::Real(f) => Ok(f),
            #[allow(clippy::cast_precision_loss)]
            Value::Integer(i) => Ok(i as f64),
            other => Err(format!("欄位是{}，不是浮點數", kind_name(&other))),
        }
    }
}
impl FromValue for String {
    fn from_value(v: Value) -> Result<Self, String> {
        match v {
            Value::Text(s) => Ok(s),
            // SQLite 的無型別欄位可以把數值讀成文字的地方（`meta.value`）在 PostgreSQL 都是 TEXT；
            // 這裡只處理運算式的結果是整數的情況
            Value::Integer(i) => Ok(i.to_string()),
            other => Err(format!("欄位是{}，不是文字", kind_name(&other))),
        }
    }
}
impl FromValue for Vec<u8> {
    fn from_value(v: Value) -> Result<Self, String> {
        match v {
            Value::Blob(b) => Ok(b),
            other => Err(format!("欄位是{}，不是位元組", kind_name(&other))),
        }
    }
}
impl<const N: usize> FromValue for [u8; N] {
    fn from_value(v: Value) -> Result<Self, String> {
        let b = Vec::<u8>::from_value(v)?;
        <[u8; N]>::try_from(b.as_slice())
            .map_err(|_| format!("位元組欄位的長度是 {}，不是 {N}", b.len()))
    }
}
impl FromValue for Value {
    fn from_value(v: Value) -> Result<Self, String> {
        Ok(v)
    }
}
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(v: Value) -> Result<Self, String> {
        match v {
            Value::Null => Ok(None),
            other => T::from_value(other).map(Some),
        }
    }
}

/// 欄位的索引：位置（從 0 起）或名稱
pub trait RowIndex: Copy + fmt::Display {
    fn find(self, row: &postgres::Row) -> Option<usize>;
}
impl RowIndex for usize {
    fn find(self, row: &postgres::Row) -> Option<usize> {
        (self < row.len()).then_some(self)
    }
}
impl RowIndex for &str {
    fn find(self, row: &postgres::Row) -> Option<usize> {
        row.columns().iter().position(|c| c.name() == self)
    }
}

/// 查詢結果的一列
pub struct Row(postgres::Row);

impl Row {
    /// 讀出一個欄位（`r.get(0)?`、`r.get::<_, Option<i64>>(3)?`、`r.get("name")?`）
    pub fn get<I: RowIndex, T: FromValue>(&self, idx: I) -> PgResult<T> {
        let pos = idx
            .find(&self.0)
            .ok_or_else(|| PgError::other(format!("查詢結果沒有欄位 {idx}")))?;
        let cell: RawCell<'_> = self.0.try_get(pos)?;
        let value = cell
            .to_value()
            .map_err(|e| PgError::other(format!("欄位 {idx}: {e}")))?;
        T::from_value(value).map_err(|e| {
            PgError::other(format!(
                "欄位 {idx}（{}）: {e}",
                self.0.columns()[pos].name()
            ))
        })
    }

    /// 欄位數
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// `conn.prepare(sql)` 的結果。敘述實際上在第一次執行時才準備（並快取在連線上）
pub struct Stmt<'c> {
    conn: &'c PgConn,
    sql: String,
}

impl Stmt<'_> {
    /// 執行查詢並逐列轉換；回傳的迭代器已持有所有結果（`rows.collect()`）
    pub fn query_map<T, P: Params, F: FnMut(&Row) -> PgResult<T>>(
        &mut self,
        params: P,
        mut f: F,
    ) -> PgResult<std::vec::IntoIter<PgResult<T>>> {
        let rows = self.conn.query_rows(&self.sql, params)?;
        Ok(rows.iter().map(&mut f).collect::<Vec<_>>().into_iter())
    }

    pub fn query_row<T, P: Params, F: FnOnce(&Row) -> PgResult<T>>(
        &mut self,
        params: P,
        f: F,
    ) -> PgResult<T> {
        self.conn.query_row(&self.sql, params, f)
    }

    pub fn execute<P: Params>(&mut self, params: P) -> PgResult<usize> {
        self.conn.execute(&self.sql, params)
    }
}

// ---------------------------------------------------------------------------------------------
// 佔位符
// ---------------------------------------------------------------------------------------------

/// 把 SQLite 形式的佔位符（`?1`、`?2`，或沒有編號、依序的 `?`）換成 PostgreSQL 的 `$1`、`$2`。
/// 單引號字串、雙引號識別字與 `--` 註解內的 `?` 不動
pub fn translate_placeholders(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut chars = sql.chars().peekable();
    let mut auto = 0_u32;
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                out.push(c);
                // 字串中的引號以連續兩個表示，會被當成「結束再開始」，結果相同
                for d in chars.by_ref() {
                    out.push(d);
                    if d == c {
                        break;
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                out.push(c);
                for d in chars.by_ref() {
                    out.push(d);
                    if d == '\n' {
                        break;
                    }
                }
            }
            '?' => {
                let mut digits = String::new();
                while let Some(d) = chars.peek().copied().filter(char::is_ascii_digit) {
                    digits.push(d);
                    chars.next();
                }
                out.push('$');
                if digits.is_empty() {
                    auto += 1;
                    out.push_str(&auto.to_string());
                } else {
                    auto = auto.max(digits.parse().unwrap_or(0));
                    out.push_str(&digits);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// 連線
// ---------------------------------------------------------------------------------------------

/// 連線池中的一條連線（歸還時保留預備敘述的快取）
pub(super) struct RawConn {
    pub client: postgres::Client,
    pub stmts: HashMap<String, postgres::Statement>,
    /// 這條連線目前的 `default_transaction_read_only`
    pub read_only: bool,
}

/// 一條 PostgreSQL 連線：自動提交模式下的 [`Store`]。drop 時歸還連線池（已中斷的連線直接關閉）
pub struct PgConn {
    raw: RefCell<Option<RawConn>>,
    in_tx: Cell<bool>,
    broken: Cell<bool>,
    /// [`TxMode::Immediate`] 等待寫入鎖的上限
    busy: Duration,
    pool: Arc<PgPool>,
    /// 來自連線池（drop 時歸還）；否則是專用連線（drop 時關閉）
    pooled: bool,
    /// 持有 session 層級的 advisory lock（歸還連線池前釋放）
    session_locked: Cell<bool>,
    /// 唯讀連線（`default_transaction_read_only = on`）：不做 fencing（唯讀交易不能 `FOR SHARE`，也不會寫入）
    read_only: bool,
    /// 不做 fencing 的連線（叢集協調自己的連線：心跳、租約的取得與續約）
    fence_exempt: Cell<bool>,
}

/// fencing 檢查失敗時 PostgreSQL 回報的 SQLSTATE（`1 / count(*)` 除以零；見 [`PgPool::set_fence`]）
const FENCE_SQLSTATE: &str = "22012";

/// 自動提交模式下會被包進 fencing 交易的敘述：`INSERT`／`UPDATE`／`DELETE` 開頭（不分大小寫）
fn is_plain_write(sql: &str) -> bool {
    let head = sql.trim_start().as_bytes();
    [&b"INSERT"[..], b"UPDATE", b"DELETE"].iter().any(|kw| {
        head.len() > kw.len()
            && head[..kw.len()].eq_ignore_ascii_case(kw)
            && head[kw.len()].is_ascii_whitespace()
    })
}

impl fmt::Debug for PgConn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgConn")
            .field("in_tx", &self.in_tx.get())
            .field("broken", &self.broken.get())
            .finish_non_exhaustive()
    }
}

impl PgConn {
    pub(super) fn new(raw: RawConn, pool: Arc<PgPool>, busy: Duration, pooled: bool) -> Self {
        Self {
            read_only: raw.read_only,
            fence_exempt: Cell::new(false),
            raw: RefCell::new(Some(raw)),
            in_tx: Cell::new(false),
            broken: Cell::new(false),
            busy,
            pool,
            pooled,
            session_locked: Cell::new(false),
        }
    }

    /// 這條連線所屬的連線池
    pub fn pool(&self) -> &Arc<PgPool> {
        &self.pool
    }

    /// 連線已中斷（之後的操作都會失敗；drop 時不歸還連線池）
    pub fn is_broken(&self) -> bool {
        self.broken.get()
    }

    /// 這條連線不做 fencing（叢集協調自己的連線：節點心跳、租約的取得、續約與交還）
    pub fn exempt_from_fencing(&self) {
        self.fence_exempt.set(true);
    }

    /// 這條連線目前要做的 fencing 檢查（SQL）：連線池設定了 fencing（這個行程是叢集的 leader）、
    /// 連線可寫、且沒有豁免時才有
    fn fence_sql(&self) -> Option<Arc<str>> {
        if self.read_only || self.fence_exempt.get() {
            return None;
        }
        self.pool.fence_sql()
    }

    /// 把 fencing 檢查的失敗（除以零）轉成明確的錯誤，並通知連線池「已失去租約」
    fn fence_failed(&self, e: PgError) -> PgError {
        if e.sqlstate() == Some(FENCE_SQLSTATE) {
            self.pool.note_fence_lost();
            PgError::other(format!(
                "{}：這個節點已不是 leader（租約已到期或已被其他節點取得），拒絕寫入",
                crate::cluster::CODE_LEASE_LOST
            ))
        } else {
            e
        }
    }

    /// 自動提交模式下的單一寫入敘述：需要 fencing 時包進「BEGIN；檢查租約；敘述；COMMIT」
    /// （`kairos/HA.md` 第 6.2 節：`FOR SHARE` 讓接手的節點等到這個交易結束）
    fn fenced_autocommit<R>(&self, sql: &str, f: impl FnOnce() -> PgResult<R>) -> PgResult<R> {
        if self.in_tx.get() || !is_plain_write(sql) {
            return f();
        }
        let Some(fence) = self.fence_sql() else {
            return f();
        };
        self.execute_batch(&format!("BEGIN; {fence}"))
            .map_err(|e| {
                // BEGIN 成功、檢查失敗時交易仍開著（且已中止）：結束它
                let _ = self.execute_batch("ROLLBACK");
                self.fence_failed(e)
            })?;
        match f() {
            Ok(r) => {
                self.end_tx("COMMIT")?;
                Ok(r)
            }
            Err(e) => {
                let _ = self.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn with_raw<R>(&self, f: impl FnOnce(&mut RawConn) -> PgResult<R>) -> PgResult<R> {
        let mut slot = self
            .raw
            .try_borrow_mut()
            .map_err(|_| PgError::other("PostgreSQL 連線正在使用中（查詢進行中不能再送出敘述）"))?;
        let raw = slot
            .as_mut()
            .ok_or_else(|| PgError::other("PostgreSQL 連線已關閉"))?;
        let result = blocking(|| f(raw));
        if let Err(e) = &result
            && (e.is_connection_fatal() || raw.client.is_closed())
        {
            self.broken.set(true);
        }
        result
    }

    fn statement(raw: &mut RawConn, sql: &str) -> PgResult<postgres::Statement> {
        if let Some(s) = raw.stmts.get(sql) {
            return Ok(s.clone());
        }
        let stmt = raw.client.prepare(&translate_placeholders(sql))?;
        if raw.stmts.len() >= STATEMENT_CACHE_LIMIT {
            raw.stmts.clear();
        }
        raw.stmts.insert(sql.to_string(), stmt.clone());
        Ok(stmt)
    }

    fn check_arity(stmt: &postgres::Statement, values: &[Value], sql: &str) -> PgResult<()> {
        if stmt.params().len() == values.len() {
            Ok(())
        } else {
            Err(PgError::other(format!(
                "敘述需要 {} 個參數，傳入 {} 個: {sql}",
                stmt.params().len(),
                values.len()
            )))
        }
    }

    /// 執行一個敘述，回傳影響的列數
    pub fn execute<P: Params>(&self, sql: &str, params: P) -> PgResult<usize> {
        let values = params.into_values();
        self.fenced_autocommit(sql, || {
            self.with_raw(|raw| {
                let stmt = Self::statement(raw, sql)?;
                Self::check_arity(&stmt, &values, sql)?;
                let refs: Vec<&(dyn ToSql + Sync)> =
                    values.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
                let n = raw.client.execute(&stmt, &refs)?;
                Ok(usize::try_from(n).unwrap_or(usize::MAX))
            })
        })
    }

    /// 執行查詢，回傳所有列
    pub fn query_rows<P: Params>(&self, sql: &str, params: P) -> PgResult<Vec<Row>> {
        let values = params.into_values();
        self.fenced_autocommit(sql, || {
            self.with_raw(|raw| {
                let stmt = Self::statement(raw, sql)?;
                Self::check_arity(&stmt, &values, sql)?;
                let mut it = raw.client.query_raw(&stmt, values.iter())?;
                let mut rows = Vec::new();
                while let Some(row) = it.next()? {
                    rows.push(Row(row));
                }
                Ok(rows)
            })
        })
    }

    /// 執行查詢並逐列交給 `f`，不把結果全部放進記憶體（匯出、搬移）。`f` 回傳 `false` 時停止。
    /// 進行中不能在同一條連線上送出其他敘述
    pub fn query_each<P: Params>(
        &self,
        sql: &str,
        params: P,
        mut f: impl FnMut(&Row) -> PgResult<bool>,
    ) -> PgResult<()> {
        let values = params.into_values();
        self.with_raw(|raw| {
            let stmt = Self::statement(raw, sql)?;
            Self::check_arity(&stmt, &values, sql)?;
            let mut it = raw.client.query_raw(&stmt, values.iter())?;
            while let Some(row) = it.next()? {
                if !f(&Row(row))? {
                    break;
                }
            }
            Ok(())
        })
    }

    /// 查詢一列（沒有列時 [`PgError::NoRows`]，搭配 `.optional()`；多於一列時取第一列）
    pub fn query_row<T, P: Params, F: FnOnce(&Row) -> PgResult<T>>(
        &self,
        sql: &str,
        params: P,
        f: F,
    ) -> PgResult<T> {
        let rows = self.query_rows(sql, params)?;
        match rows.first() {
            Some(row) => f(row),
            None => Err(PgError::NoRows),
        }
    }

    /// 與 rusqlite 相同的寫法：`conn.prepare(sql)?.query_map(params, f)?`
    pub fn prepare(&self, sql: &str) -> PgResult<Stmt<'_>> {
        Ok(Stmt {
            conn: self,
            sql: sql.to_string(),
        })
    }

    /// 執行一段（可以含多個敘述、不帶參數的）SQL
    pub fn execute_batch(&self, sql: &str) -> PgResult<()> {
        self.with_raw(|raw| Ok(raw.client.batch_execute(sql)?))
    }

    /// 在目前的交易中取得交易層級的 advisory lock（交易結束時釋放；同一個交易可以重複取得）。
    /// 必須已在交易中：自動提交模式下鎖會在這個敘述結束時立刻釋放
    pub fn lock_in_tx(&self, kind: LockKind) -> PgResult<()> {
        if !self.in_tx.get() {
            return Err(PgError::other("lock_in_tx 必須在交易中呼叫"));
        }
        let key = self.pool.lock_key(kind);
        self.execute_batch(&format!("SELECT pg_advisory_xact_lock({key})"))
    }

    /// 取得 session 層級的共享 advisory lock（連線關閉或歸還連線池時釋放）：最多等待 `wait`
    pub fn session_lock_shared(&self, kind: LockKind, wait: Duration) -> PgResult<()> {
        let key = self.pool.lock_key(kind);
        self.session_locked.set(true);
        self.execute_batch(&format!(
            "SET lock_timeout = {}; SELECT pg_advisory_lock_shared({key}); SET lock_timeout = '30s'",
            wait.as_millis().max(1)
        ))
    }

    /// 嘗試取得 session 層級的共享 advisory lock（不等待）；回傳是否取得
    pub fn try_session_lock_shared(&self, kind: LockKind) -> PgResult<bool> {
        let key = self.pool.lock_key(kind);
        let got: bool = self.query_row(
            &format!("SELECT pg_try_advisory_lock_shared({key})"),
            (),
            |r| r.get(0),
        )?;
        if got {
            self.session_locked.set(true);
        }
        Ok(got)
    }

    /// `LISTEN <channel>`（channel 必須是不需要引號的識別字）
    pub fn listen(&self, channel: &str) -> PgResult<()> {
        self.execute_batch(&format!("LISTEN {channel}"))
    }

    /// `NOTIFY <channel>`：只用來縮短其他節點的反應時間，遺失不影響正確性（`kairos/HA.md` 第 6.4 節）
    pub fn notify(&self, channel: &str) -> PgResult<()> {
        self.execute_batch(&format!("NOTIFY {channel}"))
    }

    /// 等待 `LISTEN` 的通知，最多 `timeout`；回傳是否收到（收到時把已到達的通知全部取走）
    pub fn wait_notification(&self, timeout: Duration) -> PgResult<bool> {
        self.with_raw(|raw| {
            let mut n = raw.client.notifications();
            let got = n.timeout_iter(timeout).next()?.is_some();
            if got {
                let mut pending = n.iter();
                while pending.next()?.is_some() {}
            }
            Ok(got)
        })
    }

    /// 嘗試取得 session 層級的互斥 advisory lock（不等待）；回傳是否取得
    pub fn try_session_lock(&self, kind: LockKind) -> PgResult<bool> {
        let key = self.pool.lock_key(kind);
        let got: bool =
            self.query_row(&format!("SELECT pg_try_advisory_lock({key})"), (), |r| {
                r.get(0)
            })?;
        if got {
            self.session_locked.set(true);
        }
        Ok(got)
    }

    /// 目前在交易中
    pub fn in_transaction(&self) -> bool {
        self.in_tx.get()
    }

    /// 清掉快取的預備敘述（schema 變更之後：既有敘述的結果型別可能已經改變）
    pub fn clear_statements(&self) {
        if let Ok(mut slot) = self.raw.try_borrow_mut()
            && let Some(raw) = slot.as_mut()
        {
            raw.stmts.clear();
        }
    }

    /// 開始交易。[`TxMode::Immediate`]：另外取得寫入鎖（最多等待開啟連線時指定的 busy 時間）
    pub fn begin_tx(&self, mode: TxMode) -> PgResult<PgTx<'_>> {
        self.begin_with(mode, "")
    }

    /// 開始唯讀的 `REPEATABLE READ` 交易（同一個快照；稽核鏈驗證、搬移的來源）
    pub fn begin_snapshot(&self) -> PgResult<PgTx<'_>> {
        self.begin_with(
            TxMode::Deferred,
            " ISOLATION LEVEL REPEATABLE READ READ ONLY",
        )
    }

    fn begin_with(&self, mode: TxMode, options: &str) -> PgResult<PgTx<'_>> {
        if self.in_tx.get() {
            return Err(PgError::other(
                "cannot start a transaction within a transaction",
            ));
        }
        self.execute_batch(&format!("BEGIN{options}"))?;
        self.in_tx.set(true);
        let tx = PgTx {
            conn: self,
            done: false,
        };
        if mode == TxMode::Immediate {
            // 等不到寫入鎖時回 55P03（StoreErrorKind::Busy），與 SQLite 的 busy 相同；tx 被 drop 時回復
            let key = self.pool.lock_key(LockKind::Write);
            self.execute_batch(&format!(
                "SET LOCAL lock_timeout = {}; SELECT pg_advisory_xact_lock({key})",
                self.busy.as_millis().max(1)
            ))?;
        }
        // fencing（`kairos/HA.md` 第 6.2 節）：這個行程是叢集的 leader 時，每個可寫交易的第一個敘述都確認
        // 租約仍是自己的（`FOR SHARE`）。唯讀快照不檢查。順序：寫入鎖 → 租約列 → 業務列 → 稽核鎖
        if options.is_empty()
            && let Some(fence) = self.fence_sql()
        {
            self.execute_batch(&fence)
                .map_err(|e| self.fence_failed(e))?;
        }
        Ok(tx)
    }

    fn end_tx(&self, sql: &str) -> PgResult<()> {
        let r = self.execute_batch(sql);
        self.in_tx.set(false);
        if r.is_err() {
            // 交易的結束狀態不明：這條連線不再重複使用
            self.broken.set(true);
        }
        r
    }
}

impl Drop for PgConn {
    fn drop(&mut self) {
        let Some(mut raw) = self.raw.borrow_mut().take() else {
            return;
        };
        blocking(|| {
            if !self.pooled {
                // 專用連線：關閉（drop client 會送出 Terminate）
                drop(raw);
                return;
            }
            if self.in_tx.get() && raw.client.batch_execute("ROLLBACK").is_err() {
                self.broken.set(true);
            }
            if self.session_locked.get()
                && raw
                    .client
                    .batch_execute("SELECT pg_advisory_unlock_all()")
                    .is_err()
            {
                self.broken.set(true);
            }
            if self.broken.get() || raw.client.is_closed() {
                drop(raw);
                self.pool.discard();
            } else {
                self.pool.put_back(raw);
            }
        });
    }
}

/// 交易中的連線。沒有 [`PgTx::commit`] 就被 drop 時回復
pub struct PgTx<'c> {
    conn: &'c PgConn,
    done: bool,
}

impl PgTx<'_> {
    pub fn commit(mut self) -> PgResult<()> {
        self.done = true;
        self.conn.end_tx("COMMIT")
    }
}

impl std::ops::Deref for PgTx<'_> {
    type Target = PgConn;
    fn deref(&self) -> &PgConn {
        self.conn
    }
}

impl Drop for PgTx<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.conn.end_tx("ROLLBACK");
        }
    }
}

/// 可以取得底層 [`PgConn`] 的型別（連線本身與交易）
pub trait AsPg {
    fn pg(&self) -> &PgConn;
}

impl AsPg for PgConn {
    fn pg(&self) -> &PgConn {
        self
    }
}

impl AsPg for PgTx<'_> {
    fn pg(&self) -> &PgConn {
        self.conn
    }
}

impl Store for PgConn {
    fn begin(&self, mode: TxMode) -> StoreResult<Box<dyn Tx + '_>> {
        Ok(Box::new(self.begin_tx(mode)?))
    }

    fn is_autocommit(&self) -> bool {
        !self.in_tx.get()
    }

    #[cfg(test)]
    fn test_sqlite(&self) -> &rusqlite::Connection {
        panic!("PostgreSQL 後端沒有 SQLite 連線（這個測試只適用於 SQLite）")
    }
}

impl Store for PgTx<'_> {
    fn begin(&self, _mode: TxMode) -> StoreResult<Box<dyn Tx + '_>> {
        Err(PgError::other("cannot start a transaction within a transaction").into())
    }

    fn is_autocommit(&self) -> bool {
        false
    }

    #[cfg(test)]
    fn test_sqlite(&self) -> &rusqlite::Connection {
        panic!("PostgreSQL 後端沒有 SQLite 連線（這個測試只適用於 SQLite）")
    }
}

impl Tx for PgTx<'_> {
    fn commit(self: Box<Self>) -> StoreResult<()> {
        Ok(PgTx::commit(*self)?)
    }
}

/// 把領域 trait 的方法轉呼叫到 PostgreSQL 實作的函式：`fn 名稱(&self, 參數: 型別, …) -> 回傳 => 函式;`。
/// 函式的第一個參數是 `&PgConn`，回傳 `PgResult<回傳>`
macro_rules! pg_forward {
    ($( $(#[$m:meta])* fn $name:ident(&self $(, $arg:ident : $ty:ty)* $(,)?) -> $ret:ty => $path:path; )*) => {
        $(
            $(#[$m])*
            fn $name(&self $(, $arg: $ty)*) -> $crate::store::db::StoreResult<$ret> {
                Ok($path($crate::store::postgres::conn::AsPg::pg(self) $(, $arg)*)?)
            }
        )*
    };
}
pub(crate) use pg_forward;

/// 為 [`PgConn`] 與 [`PgTx`] 實作一個領域 trait（內容與 [`pg_forward`] 相同）：
/// `pg_domain! { TokenStore { fn …(&self, …) -> … => 函式; … } }`
macro_rules! pg_domain {
    ($trait:path { $($body:tt)* }) => {
        impl $trait for $crate::store::postgres::conn::PgConn {
            $crate::store::postgres::conn::pg_forward! { $($body)* }
        }
        impl $trait for $crate::store::postgres::conn::PgTx<'_> {
            $crate::store::postgres::conn::pg_forward! { $($body)* }
        }
    };
}
pub(crate) use pg_domain;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_translated_outside_literals() {
        assert_eq!(
            translate_placeholders("SELECT a FROM t WHERE b = ?1 AND c = ?2 AND d = ?1"),
            "SELECT a FROM t WHERE b = $1 AND c = $2 AND d = $1"
        );
        assert_eq!(
            translate_placeholders("INSERT INTO t VALUES (?, ?, ?)"),
            "INSERT INTO t VALUES ($1, $2, $3)"
        );
        assert_eq!(
            translate_placeholders(
                "SELECT '?1', \"a?\" FROM t -- why?\nWHERE x = ?10 AND y LIKE 'it''s ?' AND z = ?"
            ),
            "SELECT '?1', \"a?\" FROM t -- why?\nWHERE x = $10 AND y LIKE 'it''s ?' AND z = $11"
        );
    }

    #[test]
    fn numeric_is_decoded() {
        // 12345678 = 1234 × 10000 + 5678：ndigits 2、weight 1
        let raw = [0, 2, 0, 1, 0, 0, 0, 0, 0x04, 0xD2, 0x16, 0x2E];
        assert_eq!(numeric_to_f64(&raw).unwrap(), 12_345_678.0);
        // -1.5：ndigits 2、weight 0、負號、digits [1, 5000]
        let raw = [0, 2, 0, 0, 0x40, 0, 0, 1, 0, 1, 0x13, 0x88];
        assert_eq!(numeric_to_f64(&raw).unwrap(), -1.5);
        // 0：沒有位數
        assert_eq!(numeric_to_f64(&[0, 0, 0, 0, 0, 0, 0, 0]).unwrap(), 0.0);
    }

    #[test]
    fn values_convert_between_rust_types() {
        assert_eq!(i64::from_value(Value::Integer(7)).unwrap(), 7);
        assert_eq!(u32::from_value(Value::Text(" 42 ".into())).unwrap(), 42);
        assert!(u8::from_value(Value::Integer(300)).is_err());
        assert!(bool::from_value(Value::Integer(1)).unwrap());
        assert_eq!(Option::<i64>::from_value(Value::Null).unwrap(), None);
        assert_eq!(f64::from_value(Value::Integer(2)).unwrap(), 2.0);
        assert!(String::from_value(Value::Blob(vec![1])).is_err());
        assert_eq!(
            params![1_i64, "a", Some(2_u32), None::<String>, true].len(),
            5
        );
        assert_eq!(
            (1_i64, "x").into_values(),
            vec![Value::Integer(1), Value::Text("x".into())]
        );
        assert_eq!(
            array([1_i64, 2]),
            Value::Array(vec![Value::Integer(1), Value::Integer(2)])
        );
    }
}
