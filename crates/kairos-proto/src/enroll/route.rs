//! 8444 的路徑（ENROLL.md「端點一覽」）。Kairos 以 [`parse_route`] 做路徑白名單，Pulse 以
//! [`Route::path`] 組出請求路徑，兩邊不會各寫一份字串。

use super::codes::EndpointClass;
use crate::is_uuid_text;

/// 註冊 API 的路徑前綴
pub const ENROLL_API_PREFIX: &str = "/v1/enroll";
/// 安裝腳本與套件下載的路徑前綴
pub const DOWNLOAD_PREFIX: &str = "/agent/";

/// 8444 的端點
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `GET /v1/enroll/server-info`
    ServerInfo,
    /// `POST /v1/enroll/token-check`
    TokenCheck,
    /// `POST /v1/enroll/requests`：新註冊
    CreateRequest,
    /// `POST /v1/enroll/renewals`：過期寬限期續簽
    CreateRenewal,
    /// `GET /v1/enroll/requests/{request_id}`：輪詢、取回憑證
    Poll(String),
    /// `POST /v1/enroll/requests/{request_id}/withdraw/challenge`
    WithdrawChallenge(String),
    /// `POST /v1/enroll/requests/{request_id}/withdraw`
    Withdraw(String),
    /// `GET /agent/<檔名>`：安裝腳本與套件
    Download(String),
}

/// 路徑不在白名單
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteError {
    /// 404 `not_found`
    NotFound,
    /// 405 `method_not_allowed`，附 `Allow` 標頭的值
    MethodNotAllowed(&'static str),
}

impl Route {
    /// HTTP 方法
    pub fn method(&self) -> &'static str {
        match self {
            Self::ServerInfo | Self::Poll(_) | Self::Download(_) => "GET",
            _ => "POST",
        }
    }

    /// 路徑（不含查詢字串）
    pub fn path(&self) -> String {
        match self {
            Self::ServerInfo => format!("{ENROLL_API_PREFIX}/server-info"),
            Self::TokenCheck => format!("{ENROLL_API_PREFIX}/token-check"),
            Self::CreateRequest => format!("{ENROLL_API_PREFIX}/requests"),
            Self::CreateRenewal => format!("{ENROLL_API_PREFIX}/renewals"),
            Self::Poll(id) => format!("{ENROLL_API_PREFIX}/requests/{id}"),
            Self::WithdrawChallenge(id) => {
                format!("{ENROLL_API_PREFIX}/requests/{id}/withdraw/challenge")
            }
            Self::Withdraw(id) => format!("{ENROLL_API_PREFIX}/requests/{id}/withdraw"),
            Self::Download(name) => format!("{DOWNLOAD_PREFIX}{name}"),
        }
    }

    /// 模式白名單與速率限制的分類
    pub fn class(&self) -> EndpointClass {
        match self {
            Self::ServerInfo => EndpointClass::ServerInfo,
            Self::TokenCheck => EndpointClass::TokenCheck,
            Self::CreateRequest => EndpointClass::NewEnrollment,
            Self::CreateRenewal => EndpointClass::GraceRenewal,
            Self::Poll(_) => EndpointClass::Poll,
            Self::WithdrawChallenge(_) | Self::Withdraw(_) => EndpointClass::Withdraw,
            Self::Download(_) => EndpointClass::Download,
        }
    }
}

/// 下載檔名：1～128 個英文字母、數字或 `. _ -`，不以 `.` 開頭（不允許子目錄與 `..`）
fn is_download_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// 依方法與路徑（不含查詢字串）判定端點。`HEAD` 視同 `GET`。路徑中的 `request_id` 必須是 UUID
/// （大小寫皆可，回傳時轉小寫），否則 `NotFound`；不接受結尾的 `/`、重複的 `/` 與百分比編碼
pub fn parse_route(method: &str, path: &str) -> Result<Route, RouteError> {
    let route = if let Some(name) = path.strip_prefix(DOWNLOAD_PREFIX) {
        if !is_download_name(name) {
            return Err(RouteError::NotFound);
        }
        Route::Download(name.to_string())
    } else {
        let rest = path
            .strip_prefix(ENROLL_API_PREFIX)
            .and_then(|r| r.strip_prefix('/'))
            .ok_or(RouteError::NotFound)?;
        let parts: Vec<&str> = rest.split('/').collect();
        let id = |s: &str| {
            is_uuid_text(s)
                .then(|| s.to_ascii_lowercase())
                .ok_or(RouteError::NotFound)
        };
        match parts.as_slice() {
            ["server-info"] => Route::ServerInfo,
            ["token-check"] => Route::TokenCheck,
            ["requests"] => Route::CreateRequest,
            ["renewals"] => Route::CreateRenewal,
            ["requests", rid] => Route::Poll(id(rid)?),
            ["requests", rid, "withdraw", "challenge"] => Route::WithdrawChallenge(id(rid)?),
            ["requests", rid, "withdraw"] => Route::Withdraw(id(rid)?),
            _ => return Err(RouteError::NotFound),
        }
    };
    let method = if method == "HEAD" { "GET" } else { method };
    if method == route.method() {
        Ok(route)
    } else if route.method() == "GET" {
        Err(RouteError::MethodNotAllowed("GET, HEAD"))
    } else {
        Err(RouteError::MethodNotAllowed("POST"))
    }
}
