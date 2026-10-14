//! 8444 用的最小 HTTP/1.1 用戶端（ENROLL.md 第 2 節：只有 HTTP/1.1、keep-alive、JSON）。
//!
//! 只實作註冊通道需要的部分：一次一個請求（不 pipelining）、`Content-Length` 或 `chunked` 的回應本文、
//! `Connection: close` 與伺服器關閉連線的偵測。不引入 hyper：Pulse 的請求固定、數量少，
//! 自己寫可以把逾時與上限控制在這裡，也不增加 Windows 交叉編譯的依賴。

use std::io;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// 回應標頭的大小上限
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// 回應本文的大小上限（`issued` 的回應含憑證 PEM，約 2～8 KB）
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// 一則回應
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// 標頭（名稱已轉小寫）
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// 伺服器表示要關閉連線（或 HTTP/1.0、以關閉連線結束本文）
    pub close: bool,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// `Retry-After`（整數秒）
    pub fn retry_after(&self) -> Option<Duration> {
        self.header("retry-after")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
    }

    /// 本文是 JSON（`Content-Type: application/json`，可帶參數；測試用）
    #[cfg(test)]
    pub fn is_json(&self) -> bool {
        self.header("content-type").is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
        })
    }
}

/// 一條 HTTP/1.1 連線
pub struct HttpConn<S> {
    io: BufReader<S>,
    host: String,
    reusable: bool,
}

impl<S: AsyncRead + AsyncWrite + Unpin> HttpConn<S> {
    /// `host` 為 `Host` 標頭的值（`host:port`）
    pub fn new(stream: S, host: impl Into<String>) -> Self {
        Self {
            io: BufReader::new(stream),
            host: host.into(),
            reusable: true,
        }
    }

    /// 這條連線還能送下一個請求
    pub fn is_reusable(&self) -> bool {
        self.reusable
    }

    /// 取得底層串流（關閉連線用）
    pub fn get_mut(&mut self) -> &mut S {
        self.io.get_mut()
    }

    /// 送出請求並讀完回應。`timeout` 涵蓋送出與讀完整個回應
    pub async fn send(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> io::Result<Response> {
        match tokio::time::timeout(timeout, self.send_inner(method, path, headers, body)).await {
            Ok(res) => {
                if res.is_err() {
                    self.reusable = false;
                }
                res
            }
            Err(_) => {
                self.reusable = false;
                Err(io::Error::new(io::ErrorKind::TimedOut, "等待回應逾時"))
            }
        }
    }

    async fn send_inner(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> io::Result<Response> {
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\n", self.host);
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(b) = body {
            req.push_str("Content-Type: application/json\r\n");
            req.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        req.push_str("\r\n");
        let mut bytes = req.into_bytes();
        if let Some(b) = body {
            bytes.extend_from_slice(b);
        }
        let stream = self.io.get_mut();
        stream.write_all(&bytes).await?;
        stream.flush().await?;
        let response = read_response(&mut self.io, method == "HEAD").await?;
        if response.close {
            self.reusable = false;
        }
        Ok(response)
    }
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// 讀一行（CRLF 或 LF 結尾），累計標頭大小不超過上限
async fn read_line<R: AsyncBufReadExt + Unpin>(
    r: &mut R,
    budget: &mut usize,
) -> io::Result<String> {
    let mut line = Vec::new();
    let n = (&mut *r)
        .take(*budget as u64 + 1)
        .read_until(b'\n', &mut line)
        .await?;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "伺服器在回應前關閉了連線",
        ));
    }
    if n > *budget || !line.ends_with(b"\n") {
        return Err(invalid("回應標頭過長"));
    }
    *budget -= n;
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    String::from_utf8(line).map_err(|_| invalid("回應標頭不是 UTF-8"))
}

/// 讀取一則回應（狀態列、標頭、本文）
pub async fn read_response<R: AsyncRead + Unpin>(
    r: &mut BufReader<R>,
    head_only: bool,
) -> io::Result<Response> {
    let mut budget = MAX_HEADER_BYTES;
    let status_line = read_line(r, &mut budget).await?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(invalid(format!(
            "不是 HTTP/1.x 回應：{}",
            status_line.chars().take(40).collect::<String>()
        )));
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .filter(|s| (100..=599).contains(s))
        .ok_or_else(|| invalid("回應的狀態碼不合法"))?;
    let mut headers = Vec::new();
    loop {
        let line = read_line(r, &mut budget).await?;
        if line.is_empty() {
            break;
        }
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| invalid("回應標頭格式錯誤"))?;
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    let mut close = version == "HTTP/1.0"
        || get("connection")
            .is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close")));
    let no_body = head_only || status == 204 || status == 304 || (100..200).contains(&status);
    let body = if no_body {
        Vec::new()
    } else if get("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        read_chunked(r).await?
    } else if let Some(len) = get("content-length") {
        let len: usize = len
            .trim()
            .parse()
            .map_err(|_| invalid("Content-Length 不合法"))?;
        if len > MAX_BODY_BYTES {
            return Err(invalid(format!("回應本文超過 {MAX_BODY_BYTES} bytes")));
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body).await?;
        body
    } else {
        // 沒有長度：讀到連線關閉為止
        close = true;
        let mut body = Vec::new();
        (&mut *r)
            .take(MAX_BODY_BYTES as u64 + 1)
            .read_to_end(&mut body)
            .await?;
        if body.len() > MAX_BODY_BYTES {
            return Err(invalid(format!("回應本文超過 {MAX_BODY_BYTES} bytes")));
        }
        body
    };
    Ok(Response {
        status,
        headers,
        body,
        close,
    })
}

async fn read_chunked<R: AsyncRead + Unpin>(r: &mut BufReader<R>) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut budget = 1024;
        let line = read_line(r, &mut budget).await?;
        let size_text = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| invalid("chunk 長度不合法"))?;
        if size == 0 {
            // trailer（略過）直到空行
            let mut budget = MAX_HEADER_BYTES;
            while !read_line(r, &mut budget).await?.is_empty() {}
            return Ok(body);
        }
        if body.len().saturating_add(size) > MAX_BODY_BYTES {
            return Err(invalid(format!("回應本文超過 {MAX_BODY_BYTES} bytes")));
        }
        let start = body.len();
        body.resize(start + size, 0);
        r.read_exact(&mut body[start..]).await?;
        let mut crlf = [0u8; 2];
        r.read_exact(&mut crlf).await?;
        if &crlf != b"\r\n" {
            return Err(invalid("chunk 結尾不是 CRLF"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(raw: &[u8]) -> io::Result<Response> {
        let mut r = BufReader::new(raw);
        read_response(&mut r, false).await
    }

    #[tokio::test]
    async fn parses_content_length_and_headers() {
        let r = parse(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 42\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: 2\r\n\r\n{}")
            .await
            .unwrap();
        assert_eq!(r.status, 429);
        assert_eq!(r.retry_after(), Some(Duration::from_secs(42)));
        assert!(r.is_json());
        assert_eq!(r.body, b"{}");
        assert!(!r.close);
    }

    #[tokio::test]
    async fn parses_chunked_and_close() {
        let r = parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(r.body, b"abcde");
        assert!(r.close);
        let r = parse(b"HTTP/1.0 404 Not Found\r\n\r\nnot found")
            .await
            .unwrap();
        assert_eq!(r.status, 404);
        assert_eq!(r.body, b"not found");
        assert!(r.close);
    }

    #[tokio::test]
    async fn rejects_malformed_responses() {
        assert!(parse(b"").await.is_err());
        assert!(parse(b"SSH-2.0-OpenSSH\r\n\r\n").await.is_err());
        assert!(parse(b"HTTP/1.1 abc OK\r\n\r\n").await.is_err());
        assert!(
            parse(b"HTTP/1.1 200 OK\r\nContent-Length: 9999999\r\n\r\n")
                .await
                .is_err()
        );
        assert!(
            parse(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nab")
                .await
                .is_err()
        );
        let mut huge = b"HTTP/1.1 200 OK\r\nX: ".to_vec();
        huge.extend(std::iter::repeat_n(b'a', MAX_HEADER_BYTES + 10));
        huge.extend_from_slice(b"\r\n\r\n");
        assert!(parse(&huge).await.is_err());
    }

    #[tokio::test]
    async fn keep_alive_round_trip_over_a_duplex_stream() {
        let (client, server) = tokio::io::duplex(4096);
        let srv = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            for _ in 0..2 {
                let mut req = String::new();
                loop {
                    let mut line = String::new();
                    server.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    req.push_str(&line);
                }
                let len: usize = req
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .map_or(0, |v| v.trim().parse().unwrap());
                let mut body = vec![0; len];
                server.read_exact(&mut body).await.unwrap();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    String::from_utf8(body).unwrap()
                );
                server.get_mut().write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let mut conn = HttpConn::new(client, "kairos.example.gov.tw:8444");
        for body in ["{\"a\":1}", "{\"b\":2}"] {
            let r = conn
                .send(
                    "POST",
                    "/v1/enroll/token-check",
                    &[],
                    Some(body.as_bytes()),
                    Duration::from_secs(5),
                )
                .await
                .unwrap();
            assert_eq!(r.body, body.as_bytes());
            assert!(conn.is_reusable());
        }
        srv.await.unwrap();
    }
}
