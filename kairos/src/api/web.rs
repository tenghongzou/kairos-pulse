//! 主控台的網頁介面（審計 F2；設計包 `kairos/screens.md`）：靜態檔的提供與安全標頭。
//!
//! - 檔案在編譯時由 `build.rs` 嵌入（`kairos/web/index.html` 與 `kairos/web/assets/**`），執行時不讀檔案系統。
//! - `/api/…` 是 API（`kairos/API.md`），`/healthz`、`/readyz`、`/metrics` 是健康檢查（`crate::probe`），
//!   兩者都不經過這裡；其餘路徑：
//!   - `/assets/<檔案>`：嵌入的檔案，帶 `ETag`（內容的 SHA-256），`Cache-Control: no-cache`
//!     （每次以 `If-None-Match` 重新確認，換版後立即生效，不需要在檔名加雜湊）；不存在時 404。
//!   - 最後一段含 `.` 的路徑（`/favicon.ico`、`/robots.txt`）：404（不是頁面）；網頁介面路由第二段以後的參數例外（帳號 `alice.w`）。
//!   - 其他路徑（`/`、`/hosts`、`/hosts/<agent_id>/metrics`、`/login`…）：單頁應用的 `index.html`，
//!     `Cache-Control: no-store`，路由由瀏覽器端決定（不存在的頁面由前端顯示 404 頁）。第一段不是主控台的
//!     路由（[`SPA_ROOTS`]）時同樣回 `index.html`，但狀態碼為 404（例如 Pulse 把註冊端點誤設為主控台的連接埠）。
//! - 只接受 `GET`／`HEAD`（其他 405）。
//! - 安全標頭（[`apply_security_headers`]）加在主控台的**每個**回應上：HTML 有完整的 CSP
//!   （不允許 inline script、`eval`、inline style，`frame-ancestors 'none'`，並要求 Trusted Types）；
//!   API 與其他回應為 `default-src 'none'`。主控台只有 HTTPS，所以一律帶 HSTS。

use hyper::body::Bytes;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Response, StatusCode};
use std::collections::HashMap;
use std::sync::LazyLock;

use super::http::ApiBody;

mod generated {
    include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));
}

/// 網頁（`index.html`）的 Content-Security-Policy
///
/// - `script-src 'self'`：只有同源的檔案；不允許 inline script 與 `eval`。
/// - `style-src 'self'`：不允許 `<style>` 與 `style="…"` 屬性（程式以 CSSOM 設定的樣式不受影響）。
/// - `connect-src 'self'`：API 與 SSE 都同源。
/// - `require-trusted-types-for 'script'`、`trusted-types 'none'`：瀏覽器拒絕把字串指定給
///   `innerHTML` 等 DOM XSS 的入口（介面一律以 `textContent`／DOM API 建立內容）。
pub const CSP_HTML: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
    img-src 'self'; font-src 'self'; connect-src 'self'; manifest-src 'self'; base-uri 'none'; \
    form-action 'self'; frame-ancestors 'none'; require-trusted-types-for 'script'; \
    trusted-types 'none'";
/// API、靜態檔與錯誤回應的 CSP（不是網頁，不需要載入任何東西）
pub const CSP_OTHER: &str = "default-src 'none'; frame-ancestors 'none'";
/// HSTS：主控台只有 HTTPS。不加 `includeSubDomains`（主控台可能與其他服務共用網域）
pub const HSTS: &str = "max-age=31536000";

/// 一個嵌入的檔案
#[derive(Debug)]
pub struct Asset {
    pub body: &'static [u8],
    pub content_type: &'static str,
    /// `"<SHA-256 前 16 bytes 的十六進位>"`（含引號）
    pub etag: String,
}

/// 依副檔名決定 Content-Type
pub fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn etag_of(body: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, body);
    let hex: String = digest.as_ref()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("\"{hex}\"")
}

/// 嵌入的檔案（路徑相對於 `kairos/web/`，例如 `index.html`、`assets/js/main.js`）
pub static ASSETS: LazyLock<HashMap<&'static str, Asset>> = LazyLock::new(|| {
    generated::FILES
        .iter()
        .map(|(path, body)| {
            (
                *path,
                Asset {
                    body,
                    content_type: content_type(path),
                    etag: etag_of(body),
                },
            )
        })
        .collect()
});

/// 嵌入的檔案總大小（bytes；測試用）
#[cfg(test)]
pub fn total_bytes() -> usize {
    generated::FILES.iter().map(|(_, b)| b.len()).sum()
}

/// 這個路徑由網頁介面處理（不是 API、不是健康檢查）
pub fn is_web_path(path: &str) -> bool {
    !(path == "/api" || path.starts_with("/api/") || crate::probe::is_probe_path(path))
}

/// 主控台回應的安全標頭；`html` 為網頁（`index.html`）
pub fn apply_security_headers(headers: &mut HeaderMap, html: bool) {
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(if html { CSP_HTML } else { CSP_OTHER }),
    );
    headers.insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static(HSTS),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        "cross-origin-opener-policy",
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=(), payment=(), usb=()"),
    );
}

fn plain(status: StatusCode, text: &'static str) -> Response<ApiBody> {
    let mut resp = Response::new(ApiBody::Full(Some(Bytes::from_static(text.as_bytes()))));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn asset_response(
    asset: &Asset,
    req_headers: &HeaderMap,
    cache: &'static str,
) -> Response<ApiBody> {
    let not_modified = req_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .map(|t| t.trim().trim_start_matches("W/"))
                .any(|t| t == asset.etag || t == "*")
        });
    let mut resp = if not_modified {
        let mut r = Response::new(ApiBody::Full(None));
        *r.status_mut() = StatusCode::NOT_MODIFIED;
        r
    } else {
        let mut r = Response::new(ApiBody::Full(Some(Bytes::from_static(asset.body))));
        r.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(asset.content_type),
        );
        r
    };
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if let Ok(v) = HeaderValue::from_str(&asset.etag) {
        h.insert(header::ETAG, v);
    }
    resp
}

/// 網頁介面的路由的第一段（設計包 `ia.md` §3 網站地圖；`/` 為總覽）。其他路徑回 404（本文仍是 index.html）
pub const SPA_ROOTS: [&str; 15] = [
    "login",
    "hosts",
    "account",
    "alerts",
    "enrollment",
    "certificates",
    "audit",
    "reports",
    "admin",
    "system",
    "setup",
    "error",
    "docs",
    "help",
    "t",
];

/// 這個路徑是網頁介面的路由（第一段在 [`SPA_ROOTS`]，或是 `/`）
pub fn is_spa_route(path: &str) -> bool {
    let first = path.trim_start_matches('/').split('/').next().unwrap_or("");
    first.is_empty() || SPA_ROOTS.contains(&first)
}

/// 處理網頁介面的請求（[`is_web_path`] 為真的路徑）；回傳的回應尚未加上安全標頭
pub fn serve(method: &Method, path: &str, req_headers: &HeaderMap) -> (Response<ApiBody>, bool) {
    if method != Method::GET && method != Method::HEAD {
        let mut r = plain(StatusCode::METHOD_NOT_ALLOWED, "405 method not allowed\n");
        r.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return (r, false);
    }
    if let Some(rest) = path.strip_prefix("/assets/") {
        let key = format!("assets/{rest}");
        return match ASSETS.get(key.as_str()) {
            Some(a) => (asset_response(a, req_headers, "no-cache"), false),
            None => (plain(StatusCode::NOT_FOUND, "404 not found\n"), false),
        };
    }
    let last = path.rsplit('/').next().unwrap_or("");
    // 含 `.` 的最後一段多半是檔案（/favicon.ico、/robots.txt）；但網頁介面路由下第二段以後的參數可能含 `.`
    // （帳號 `alice.w`：/admin/users/alice.w），這些仍回 index.html
    let segments = path.trim_matches('/').split('/').count();
    if last.contains('.') && !(segments >= 2 && is_spa_route(path)) {
        return (plain(StatusCode::NOT_FOUND, "404 not found\n"), false);
    }
    match ASSETS.get("index.html") {
        Some(a) => {
            let mut r = asset_response(a, req_headers, "no-store");
            // 不是主控台的路由（例如把 Pulse 的註冊端點誤設為主控台的連接埠時的 /v1/enroll/…）：
            // 仍回 index.html（瀏覽器顯示 404 頁），但狀態碼是 404，程式不會誤以為成功
            if r.status() == StatusCode::OK && !is_spa_route(path) {
                *r.status_mut() = StatusCode::NOT_FOUND;
            }
            (r, true)
        }
        None => (
            plain(
                StatusCode::NOT_FOUND,
                "這個 Kairos 執行檔沒有內含網頁介面（建置時 kairos/web/ 不存在）\n",
            ),
            false,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_and_assets_are_embedded_with_etags() {
        let index = ASSETS.get("index.html").expect("index.html 應已嵌入");
        assert_eq!(index.content_type, "text/html; charset=utf-8");
        assert!(index.etag.starts_with('"') && index.etag.len() == 34);
        assert!(ASSETS.contains_key("assets/tokens.css"));
        assert!(ASSETS.contains_key("assets/app.css"));
        assert!(ASSETS.contains_key("assets/js/main.js"));
        // 測試、工具與說明不嵌入
        assert!(
            ASSETS
                .keys()
                .all(|k| !k.contains("tests/") && !k.contains("tools/"))
        );
        assert!(!ASSETS.contains_key("README.md"));
        assert!(total_bytes() > 0);
    }

    #[test]
    fn index_html_has_no_inline_script_or_style() {
        let html = std::str::from_utf8(ASSETS["index.html"].body).unwrap();
        // 只有帶 src 的 <script>（CSP 不允許 inline script）
        for (i, _) in html.match_indices("<script") {
            let tag_end = html[i..].find('>').unwrap() + i;
            assert!(
                html[i..tag_end].contains("src="),
                "index.html 有 inline script：{}",
                &html[i..tag_end]
            );
        }
        assert!(!html.contains("<style"), "index.html 有 <style>");
        assert!(!html.contains("style=\""), "index.html 有 style 屬性");
        for attr in ["onclick=", "onload=", "onerror=", "javascript:"] {
            assert!(!html.contains(attr), "index.html 有 {attr}");
        }
    }

    #[test]
    fn routes() {
        assert!(is_web_path("/"));
        assert!(is_web_path(
            "/hosts/6f1c0e4a-1b2c-4d3e-9f00-112233445566/metrics"
        ));
        assert!(is_web_path("/assets/app.css"));
        assert!(!is_web_path("/api/v1/hosts"));
        assert!(!is_web_path("/api"));
        assert!(!is_web_path("/healthz"));
        assert!(!is_web_path("/metrics"));
        let h = HeaderMap::new();
        let (r, html) = serve(&Method::GET, "/hosts", &h);
        assert_eq!(r.status(), StatusCode::OK);
        assert!(html);
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
        // 不是主控台的路由：本文仍是 index.html（瀏覽器顯示 404 頁），狀態碼 404
        let (r, html) = serve(&Method::GET, "/v1/enroll/server-info", &h);
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert!(html);
        assert!(is_spa_route("/") && is_spa_route("/account") && !is_spa_route("/nope"));
        let (r, _) = serve(&Method::GET, "/favicon.ico", &h);
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let (r, _) = serve(&Method::GET, "/v1/foo.json", &h);
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        // 路由的參數可以含 `.`（帳號 alice.w）
        let (r, html) = serve(&Method::GET, "/admin/users/alice.w", &h);
        assert_eq!(r.status(), StatusCode::OK);
        assert!(html);
        let (r, _) = serve(&Method::GET, "/assets/nope.js", &h);
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let (r, _) = serve(&Method::GET, "/assets/../index.html", &h);
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let (r, _) = serve(&Method::POST, "/", &h);
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
        // ETag 相符時 304
        let etag = ASSETS["assets/app.css"].etag.clone();
        let mut h2 = HeaderMap::new();
        h2.insert(header::IF_NONE_MATCH, HeaderValue::from_str(&etag).unwrap());
        let (r, _) = serve(&Method::GET, "/assets/app.css", &h2);
        assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(r.headers()[header::ETAG], etag.as_str());
    }

    /// 尋找設計包（`design/design-system/tokens.css`）：環境變數 `KAIROS_DESIGN_DIR`，
    /// 或 `kairos/` 往上各層的 `design/`
    fn design_dir() -> Option<std::path::PathBuf> {
        if let Ok(d) = std::env::var("KAIROS_DESIGN_DIR") {
            return Some(d.into());
        }
        let mut dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        loop {
            let cand = dir.join("design");
            if cand.join("design-system").join("tokens.css").is_file() {
                return Some(cand);
            }
            if !dir.pop() {
                return None;
            }
        }
    }

    fn lf(b: &[u8]) -> Vec<u8> {
        String::from_utf8_lossy(b)
            .replace("\r\n", "\n")
            .into_bytes()
    }

    /// `kairos/web/assets/tokens.css` 是設計包 `design-system/tokens.css` 的複本：內容必須相同
    /// （設計包不在 repo 內；找不到時只檢查複本的 SHA-256 與記錄的值相同，見 `web/README.md`）
    #[test]
    fn tokens_css_matches_the_design_package() {
        let copy = lf(ASSETS["assets/tokens.css"].body);
        let recorded = include_str!("../../web/tokens.css.sha256")
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();
        let digest = ring::digest::digest(&ring::digest::SHA256, &copy);
        let hex: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, recorded,
            "web/assets/tokens.css 被修改過：它是設計包的複本，請以 web/tools/sync-design.mjs 重新複製"
        );
        match design_dir() {
            Some(d) => {
                let src = lf(&std::fs::read(d.join("design-system/tokens.css")).unwrap());
                assert!(
                    src == copy,
                    "web/assets/tokens.css 與設計包 {} 的 tokens.css 不同：以 web/tools/sync-design.mjs 重新複製",
                    d.display()
                );
            }
            None => {
                eprintln!("找不到設計包（design/design-system/tokens.css），只檢查記錄的 SHA-256")
            }
        }
    }
}
