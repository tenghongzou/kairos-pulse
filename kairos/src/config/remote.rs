//! `[remote]`：集中設定與 Pulse 遠端升級（商業審計 F8；`crates/kairos-proto/REMOTE.md` 第 7 節）。
//!
//! 需要重新啟動才生效（與其他安全相關設定相同）。0.9.0 不提供 `rollout_approval`（雙人覆核，0.10）。

use serde::{Deserialize, Serialize};
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};

/// `max_concurrent_downloads` 的範圍
pub const MAX_CONCURRENT_DOWNLOADS: u32 = 10_000;
/// `max_release_bytes` 的上限（與協定的 `artifact.size` 上限相同，128 MiB）
pub const MAX_RELEASE_BYTES: u64 = kairos_proto::mgmt::MAX_ARTIFACT_BYTES;
/// `max_release_bytes` 的下限（1 MiB）
pub const MIN_RELEASE_BYTES: u64 = 1024 * 1024;
/// `rollout_retention_days` 的範圍
pub const MAX_ROLLOUT_RETENTION_DAYS: u32 = 3650;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct RemoteSettings {
    /// 宣告 feature `config`（集中設定）；需要 SQLite 與 mTLS，且 Pulse 也宣告
    pub config_enabled: bool,
    /// 宣告 feature `upgrade`（遠端升級）；同上
    pub upgrade_enabled: bool,
    /// 發佈檔的目錄（相對路徑以資料目錄解析；建立時 0700）
    pub releases_dir: String,
    /// 匯入發佈版本時驗證簽章的信任錨（`ssh-keygen` 的 `allowed_signers` 格式）。空字串＝匯入時不驗證
    /// 簽章、只檢查格式與雜湊（主控台與系統健康警告）；信任仍由每台 Pulse 以本機的信任錨決定
    pub allowed_signers: String,
    /// 除 `release@kairos-pulse` 外另外接受的 principal（客戶以自己的金鑰重新簽署，REMOTE.md D14）
    pub extra_principals: Vec<String>,
    /// 同時經 8443 下載的主機數上限（1～10000）；超過時回 `rate_limited`
    pub max_concurrent_downloads: u32,
    /// 單一發佈檔的上限（bytes，1 MiB～128 MiB）
    pub max_release_bytes: u64,
    /// 已完成或取消的推送保留天數（1～3650）
    pub rollout_retention_days: u32,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            config_enabled: true,
            upgrade_enabled: true,
            releases_dir: "releases".to_string(),
            allowed_signers: String::new(),
            extra_principals: Vec::new(),
            max_concurrent_downloads: kairos_proto::mgmt::DEFAULT_MAX_CONCURRENT_DOWNLOADS,
            max_release_bytes: MAX_RELEASE_BYTES,
            rollout_retention_days: 400,
        }
    }
}

fn invalid(msg: String) -> Error {
    Error::new(ErrorKind::InvalidInput, msg)
}

impl RemoteSettings {
    pub fn validate(&self) -> Result<(), Error> {
        if self.releases_dir.trim().is_empty() {
            return Err(invalid(
                "remote.releases_dir 不可為空字串（預設 releases，相對於資料目錄）".to_string(),
            ));
        }
        if !(1..=MAX_CONCURRENT_DOWNLOADS).contains(&self.max_concurrent_downloads) {
            return Err(invalid(format!(
                "remote.max_concurrent_downloads 必須介於 1..={MAX_CONCURRENT_DOWNLOADS}"
            )));
        }
        if !(MIN_RELEASE_BYTES..=MAX_RELEASE_BYTES).contains(&self.max_release_bytes) {
            return Err(invalid(format!(
                "remote.max_release_bytes 必須介於 {MIN_RELEASE_BYTES}..={MAX_RELEASE_BYTES}（1 MiB～128 MiB）"
            )));
        }
        if !(1..=MAX_ROLLOUT_RETENTION_DAYS).contains(&self.rollout_retention_days) {
            return Err(invalid(format!(
                "remote.rollout_retention_days 必須介於 1..={MAX_ROLLOUT_RETENTION_DAYS}"
            )));
        }
        for p in &self.extra_principals {
            let p = p.trim();
            if p.is_empty() || p.len() > 256 || p.chars().any(|c| c.is_whitespace() || c == ',') {
                return Err(invalid(format!(
                    "remote.extra_principals 的「{p}」不合法：1～256 個字元、不含空白與逗號"
                )));
            }
        }
        Ok(())
    }

    /// 發佈檔目錄的實際路徑
    pub fn releases_path(&self, data_dir: &Path) -> PathBuf {
        let p = Path::new(self.releases_dir.trim());
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            data_dir.join(p)
        }
    }

    /// `allowed_signers` 的實際路徑（沒有設定時 `None`；相對路徑以資料目錄解析）
    pub fn allowed_signers_path(&self, data_dir: &Path) -> Option<PathBuf> {
        let s = self.allowed_signers.trim();
        if s.is_empty() {
            return None;
        }
        let p = Path::new(s);
        Some(if p.is_absolute() {
            p.to_path_buf()
        } else {
            data_dir.join(p)
        })
    }

    /// 去除空白的 `extra_principals`
    pub fn principals(&self) -> Vec<String> {
        self.extra_principals
            .iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_the_spec() {
        let r = RemoteSettings::default();
        assert!(r.config_enabled && r.upgrade_enabled);
        assert_eq!(r.max_concurrent_downloads, 100);
        assert_eq!(r.max_release_bytes, 128 * 1024 * 1024);
        assert_eq!(r.rollout_retention_days, 400);
        assert!(r.validate().is_ok());
        let data = Path::new("/var/lib/kairos");
        assert_eq!(r.releases_path(data), data.join("releases"));
        assert_eq!(r.allowed_signers_path(data), None);
    }

    #[test]
    fn invalid_values_are_rejected() {
        for r in [
            RemoteSettings {
                max_concurrent_downloads: 0,
                ..Default::default()
            },
            RemoteSettings {
                max_release_bytes: 10,
                ..Default::default()
            },
            RemoteSettings {
                rollout_retention_days: 0,
                ..Default::default()
            },
            RemoteSettings {
                releases_dir: " ".into(),
                ..Default::default()
            },
            RemoteSettings {
                extra_principals: vec!["a b".into()],
                ..Default::default()
            },
        ] {
            let e = r.validate().unwrap_err().to_string();
            assert!(e.starts_with("remote."), "{e}");
        }
    }
}
