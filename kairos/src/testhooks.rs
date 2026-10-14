//! e2e 測試掛鉤（ENROLL.md 第 19.3 節：「測試政策 1 分鐘」「以測試掛鉤縮短」）。
//!
//! **只在 debug 建置生效**（`cfg(debug_assertions)`）；發佈用的 release 建置一律忽略這些環境變數，
//! 不會因為環境中殘留的變數而改變行為。
//!
//! | 環境變數 | 作用 |
//! |---|---|
//! | `KAIROS_TEST_CERT_LIFETIME_SECS` | 簽發的用戶端憑證有效秒數（取代憑證政策的有效天數） |
//! | `KAIROS_TEST_AUTO_PAUSE_SECS` | 自動暫停新註冊的門檻秒數（取代 `enroll.auto_pause_after_hours`） |

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
