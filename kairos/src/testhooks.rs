//! e2e 測試掛鉤（ENROLL.md 第 19.3 節：「測試政策 1 分鐘」「以測試掛鉤縮短」）。
//!
//! **只在 debug 建置生效**（`cfg(debug_assertions)`）；發佈用的 release 建置一律忽略這些環境變數，
//! 不會因為環境中殘留的變數而改變行為。
//!
//! | 環境變數 | 作用 |
//! |---|---|
//! | `KAIROS_TEST_CERT_LIFETIME_SECS` | 簽發的用戶端憑證有效秒數（取代憑證政策的有效天數） |
//! | `KAIROS_TEST_AUTO_PAUSE_SECS` | 自動暫停新註冊的門檻秒數（取代 `enroll.auto_pause_after_hours`） |
//! | `KAIROS_TEST_DROP_ENROLL_RESPONSES` | 8444 的前 N 個成功的 `POST /v1/enroll/requests`：照常處理（寫入資料庫），但不送出回應、直接關閉連線（ENROLL.md 第 19.3 節 #7「回應遺失」） |
//! | `KAIROS_TEST_DROP_RENEWAL_RESPONSES` | 同上，對象為 `POST /v1/enroll/renewals`（第 19.3 節 #12） |
//! | `KAIROS_TEST_GRACE_SECS` | 過期寬限期的秒數（取代憑證政策的 `expired_renew_grace_days`；政策為 0 天時仍為 0，第 19.3 節 #14） |

#[cfg(debug_assertions)]
fn read(name: &str) -> Option<i64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
}

/// `KAIROS_TEST_CERT_LIFETIME_SECS`
pub fn cert_lifetime_secs() -> Option<i64> {
    #[cfg(debug_assertions)]
    {
        static V: std::sync::OnceLock<Option<i64>> = std::sync::OnceLock::new();
        *V.get_or_init(|| read("KAIROS_TEST_CERT_LIFETIME_SECS"))
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}

/// `KAIROS_TEST_DROP_ENROLL_RESPONSES`／`KAIROS_TEST_DROP_RENEWAL_RESPONSES`：這個請求（已處理、
/// 狀態碼 `status`）的回應是否要丟棄。只計算成功（2xx）的新註冊／過期續簽；每丟棄一次減 1
pub fn drop_enroll_response(method: &str, path: &str, status: u16) -> bool {
    #[cfg(debug_assertions)]
    {
        use std::sync::OnceLock;
        use std::sync::atomic::{AtomicI64, Ordering};
        static REQUESTS: OnceLock<AtomicI64> = OnceLock::new();
        static RENEWALS: OnceLock<AtomicI64> = OnceLock::new();
        if method != "POST" || !(200..300).contains(&status) {
            return false;
        }
        let left = match path {
            "/v1/enroll/requests" => REQUESTS.get_or_init(|| {
                AtomicI64::new(read("KAIROS_TEST_DROP_ENROLL_RESPONSES").unwrap_or(0))
            }),
            "/v1/enroll/renewals" => RENEWALS.get_or_init(|| {
                AtomicI64::new(read("KAIROS_TEST_DROP_RENEWAL_RESPONSES").unwrap_or(0))
            }),
            _ => return false,
        };
        left.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n > 0).then(|| n - 1)
        })
        .is_ok()
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = (method, path, status);
        false
    }
}

/// `KAIROS_TEST_GRACE_SECS`
pub fn grace_secs() -> Option<i64> {
    #[cfg(debug_assertions)]
    {
        static V: std::sync::OnceLock<Option<i64>> = std::sync::OnceLock::new();
        *V.get_or_init(|| read("KAIROS_TEST_GRACE_SECS"))
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}

/// `KAIROS_TEST_AUTO_PAUSE_SECS`
pub fn auto_pause_secs() -> Option<i64> {
    #[cfg(debug_assertions)]
    {
        static V: std::sync::OnceLock<Option<i64>> = std::sync::OnceLock::new();
        *V.get_or_init(|| read("KAIROS_TEST_AUTO_PAUSE_SECS"))
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}
