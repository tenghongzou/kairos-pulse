//! Pulse ↔ Kairos 協定 v1：Kairos 端的 NDJSON 框架。
//!
//! 訊息型別、序列化與欄位驗證在兩個產品共用的 `kairos-proto` crate（規格見
//! `crates/kairos-proto/PROTOCOL.md`），這裡重新匯出 Kairos 用到的部分，呼叫端仍以
//! `crate::protocol::…` 取用。本模組只包含純函式與資料型別，不做任何 I/O，方便單元測試。

use std::io::{Error, ErrorKind};
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder, LinesCodec, LinesCodecError};

pub use kairos_proto::{
    ClientMsg, ErrorCode, MAX_CONSECUTIVE_BAD_REQUESTS, MAX_LINE_LENGTH, PROTO_VERSION, Proto,
    ServerMsg, clamp_heartbeat_interval_ms, hello_seq_of_invalid, normalize_agent_id,
    offline_after_ms, parse_client_msg, server_cpu_usage, validate_client_msg, validate_hello,
    validate_report,
};

/// 從連線讀到的一個框架
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// 一行合法 UTF-8 文字（已去除 `\n` / `\r\n`）
    Line(String),
    /// 單行超過 [`MAX_LINE_LENGTH`]
    TooLong,
    /// 該行不是合法 UTF-8（該行已被丟棄，可繼續讀下一行）
    InvalidUtf8,
}

/// 包裝 `LinesCodec::new_with_max_length(65536)` 的 NDJSON codec。
///
/// `LinesCodec` 遇到超長行或非 UTF-8 時會回傳錯誤，而 `Framed` 在 decoder 回錯誤後
/// 會進入 errored 狀態：下一次 poll 回傳 `None`，且緩衝區內已收到的後續行要等到
/// 下一批資料進來才會被解碼。為了讓非 UTF-8 能以非 fatal 的 `bad_request` 處理並
/// 繼續讀取，這裡把這兩種 decoder 錯誤轉成一般的 [`Frame`]，不讓 `Framed` 進入錯誤狀態。
/// `LinesCodec` 在 UTF-8 檢查前已把該行從緩衝區切除，因此狀態保持一致。
#[derive(Debug)]
pub struct NdjsonCodec {
    inner: LinesCodec,
}

impl NdjsonCodec {
    pub fn new() -> Self {
        Self {
            inner: LinesCodec::new_with_max_length(MAX_LINE_LENGTH),
        }
    }
}

impl Default for NdjsonCodec {
    fn default() -> Self {
        Self::new()
    }
}

fn map_decoded(result: Result<Option<String>, LinesCodecError>) -> Result<Option<Frame>, Error> {
    match result {
        Ok(Some(line)) => Ok(Some(Frame::Line(line))),
        Ok(None) => Ok(None),
        Err(LinesCodecError::MaxLineLengthExceeded) => Ok(Some(Frame::TooLong)),
        // LinesCodec 的 decode 只會因 UTF-8 解碼失敗回傳 InvalidData
        Err(LinesCodecError::Io(e)) if e.kind() == ErrorKind::InvalidData => {
            Ok(Some(Frame::InvalidUtf8))
        }
        Err(LinesCodecError::Io(e)) => Err(e),
    }
}

fn map_encode_error(e: LinesCodecError) -> Error {
    match e {
        LinesCodecError::Io(e) => e,
        LinesCodecError::MaxLineLengthExceeded => {
            Error::new(ErrorKind::InvalidData, "送出的訊息超過單行上限")
        }
    }
}

impl Decoder for NdjsonCodec {
    type Item = Frame;
    type Error = Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, Error> {
        map_decoded(self.inner.decode(buf))
    }

    fn decode_eof(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, Error> {
        map_decoded(self.inner.decode_eof(buf))
    }
}

impl Encoder<String> for NdjsonCodec {
    type Error = Error;

    fn encode(&mut self, line: String, buf: &mut BytesMut) -> Result<(), Error> {
        self.inner.encode(line, buf).map_err(map_encode_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_splits_lines_and_accepts_crlf() {
        let mut codec = NdjsonCodec::new();
        let mut buf = BytesMut::from(&b"{\"a\":1}\r\n{\"b\":2}\npartial"[..]);
        assert_eq!(
            codec.decode(&mut buf).unwrap(),
            Some(Frame::Line("{\"a\":1}".into()))
        );
        assert_eq!(
            codec.decode(&mut buf).unwrap(),
            Some(Frame::Line("{\"b\":2}".into()))
        );
        assert_eq!(codec.decode(&mut buf).unwrap(), None);
    }

    #[test]
    fn codec_invalid_utf8_is_skipped_and_reading_continues() {
        let mut codec = NdjsonCodec::new();
        let mut buf = BytesMut::from(&b"\xff\xfe\n{\"ok\":true}\n"[..]);
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::InvalidUtf8));
        assert_eq!(
            codec.decode(&mut buf).unwrap(),
            Some(Frame::Line("{\"ok\":true}".into()))
        );
        assert_eq!(codec.decode(&mut buf).unwrap(), None);
    }

    #[test]
    fn codec_line_length_counts_carriage_return() {
        // T2：上限 65536 bytes 不含 `\n` 但含 `\r`
        // 65535 bytes + `\r\n` → 接受，去掉 `\r` 後為 65535 bytes
        let mut codec = NdjsonCodec::new();
        let mut ok = vec![b'a'; MAX_LINE_LENGTH - 1];
        ok.extend_from_slice(b"\r\n");
        let mut buf = BytesMut::from(&ok[..]);
        assert!(
            matches!(codec.decode(&mut buf).unwrap(), Some(Frame::Line(l)) if l.len() == MAX_LINE_LENGTH - 1)
        );

        // 65536 bytes + `\r\n` → 含 `\r` 共 65537 bytes，回報 TooLong
        let mut codec = NdjsonCodec::new();
        let mut bad = vec![b'a'; MAX_LINE_LENGTH];
        bad.extend_from_slice(b"\r\n");
        let mut buf = BytesMut::from(&bad[..]);
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::TooLong));

        // 分批到達時邊界相同
        let mut codec = NdjsonCodec::new();
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&vec![b'a'; MAX_LINE_LENGTH]);
        assert_eq!(codec.decode(&mut buf).unwrap(), None);
        buf.extend_from_slice(b"\r");
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::TooLong));
    }

    #[test]
    fn codec_line_length_limit() {
        let mut codec = NdjsonCodec::new();
        // 剛好 65536 bytes 可以接受
        let mut ok = vec![b'a'; MAX_LINE_LENGTH];
        ok.push(b'\n');
        let mut buf = BytesMut::from(&ok[..]);
        assert!(
            matches!(codec.decode(&mut buf).unwrap(), Some(Frame::Line(l)) if l.len() == MAX_LINE_LENGTH)
        );

        // 超過即回報 TooLong
        let mut codec = NdjsonCodec::new();
        let mut buf = BytesMut::from(&vec![b'a'; MAX_LINE_LENGTH + 1][..]);
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::TooLong));
    }

    #[test]
    fn codec_encode_appends_newline() {
        let mut codec = NdjsonCodec::new();
        let mut buf = BytesMut::new();
        codec
            .encode("{\"type\":\"ack\",\"seq\":1}".to_string(), &mut buf)
            .unwrap();
        assert_eq!(&buf[..], b"{\"type\":\"ack\",\"seq\":1}\n");
    }
}
