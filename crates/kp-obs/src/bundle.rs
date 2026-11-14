//! 支援診斷包（`kairos diag`、`pulse diag`）共用的部分：遮蔽後加入檔案、區塊的結果、日誌的收集與大小上限、
//! 檔案清單（不含內容）、外部命令的輸出、以 0600 寫出。
//!
//! 每個文字檔加入前都先經過 [`crate::redact::redact`]（與可選的 [`Pseudonyms`]）；[`Bundle::finish`]
//! 產生前再以 [`crate::redact::find_leak`] 檢查整個 tar，仍有疑似秘密時不產生。

use crate::archive::TarGz;
use crate::redact::{self, Counts, Pseudonyms};
use chrono::{DateTime, NaiveDate, Utc};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 一個區塊的結果（`ok`／`warning`／`skipped`／`error`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub id: &'static str,
    pub result: &'static str,
    pub note: String,
}

impl Section {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"id": self.id, "result": self.result, "note": self.note})
    }
}

/// 產生中的診斷包
#[derive(Debug)]
pub struct Bundle {
    root: String,
    pub tar: TarGz,
    pub counts: Counts,
    pub sections: Vec<Section>,
    /// 有內容因大小上限而截斷
    pub truncated: bool,
    pub pseudonyms: Option<Pseudonyms>,
}

impl Bundle {
    /// `root`：tar 中的頂層目錄（例如 `kairos-diag`）
    pub fn new(root: &str, mtime: i64, pseudonyms: Option<Pseudonyms>) -> Self {
        Self {
            root: root.to_string(),
            tar: TarGz::new(mtime),
            counts: Counts::default(),
            sections: Vec::new(),
            truncated: false,
            pseudonyms,
        }
    }

    /// 遮蔽後的文字
    pub fn clean(&mut self, content: &str) -> String {
        let mut s = redact::redact(content, &mut self.counts);
        if let Some(p) = self.pseudonyms.as_mut() {
            s = p.apply(&s, &mut self.counts);
        }
        s
    }

    /// 加入一個文字檔（先遮蔽）
    pub fn text(&mut self, path: &str, content: &str) {
        let s = self.clean(content);
        let _ = self
            .tar
            .add(&format!("{}/{path}", self.root), s.into_bytes(), 0o600);
    }

    pub fn section(&mut self, id: &'static str, result: &'static str, note: impl Into<String>) {
        self.sections.push(Section {
            id,
            result,
            note: note.into(),
        });
    }

    /// 是否有警告、錯誤或截斷
    pub fn has_warnings(&self) -> bool {
        self.truncated
            || self
                .sections
                .iter()
                .any(|s| s.result == "warning" || s.result == "error")
    }

    /// 收集日誌檔（依日期由新到舊，合計最多 `max_bytes`；超過時較舊的不收、最後一個只收尾端）。
    /// 回傳 (收集的檔案數, 位元組數)
    pub fn logs(&mut self, files: &[(NaiveDate, PathBuf, u64)], max_bytes: u64) -> (usize, u64) {
        let mut total = 0u64;
        let mut collected = 0usize;
        for (_, path, size) in files {
            let left = max_bytes.saturating_sub(total);
            if left == 0 {
                self.truncated = true;
                continue;
            }
            let budget = left.min(*size);
            match read_tail(path, budget) {
                Ok((s, cut)) => {
                    // 以讀取的量計入上限（截斷後去掉的半行也算）
                    total += budget;
                    collected += 1;
                    if cut || *size > left {
                        self.truncated = true;
                    }
                    let name = path
                        .file_name()
                        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                    self.text(&format!("logs/{name}"), &s);
                }
                Err(e) => self.section(
                    "logs",
                    "warning",
                    format!("無法讀取 {}：{e}", path.display()),
                ),
            }
        }
        (collected, total)
    }

    /// 加上 `REDACTION.txt` 與 `MANIFEST.json`（`manifest` 由產品組成，傳入未壓縮 tar 的內容），
    /// 檢查沒有疑似秘密後回傳 tar.gz
    pub fn finish(
        mut self,
        hosts_redacted: bool,
        manifest: impl FnOnce(&[u8], &Bundle) -> serde_json::Value,
    ) -> Result<Vec<u8>, String> {
        let mut redaction = String::from("# 遮蔽的類別與處數（不含原值）\n");
        for (k, v) in self.counts.items() {
            redaction.push_str(&format!("{k}\t{v}\n"));
        }
        redaction.push_str(if hosts_redacted {
            "# 主機名稱與 IP 已換成代號（對照表另存在本機，不在診斷包中）\n"
        } else {
            "# 主機名稱與 IP 保留\n"
        });
        redaction.push_str("# 私鑰檔的內容一律不收集\n");
        let _ = self
            .tar
            .add(&format!("{}/REDACTION.txt", self.root), redaction, 0o600);
        let tar = self.tar.tar().map_err(|e| e.to_string())?;
        if let Some(kind) = redact::find_leak(&String::from_utf8_lossy(&tar)) {
            return Err(format!(
                "內容中仍有疑似秘密（{kind}），為安全起見不寫出診斷包"
            ));
        }
        let m = manifest(&tar, &self);
        let _ = self.tar.add(
            &format!("{}/MANIFEST.json", self.root),
            serde_json::to_string_pretty(&m).unwrap_or_default(),
            0o600,
        );
        self.tar.finish().map_err(|e| e.to_string())
    }

    /// 遮蔽的處數（MANIFEST 用）
    pub fn removed_json(&self) -> serde_json::Value {
        serde_json::Value::Object(
            self.counts
                .items()
                .iter()
                .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
                .collect(),
        )
    }
}

/// `dir` 中符合 `parse`（檔名 → 日期）、日期不早於 `now − since` 的日誌檔：(日期, 路徑, 大小)，由新到舊
pub fn log_files(
    dir: &Path,
    parse: impl Fn(&str) -> Option<NaiveDate>,
    since: Duration,
    now: DateTime<Utc>,
) -> Vec<(NaiveDate, PathBuf, u64)> {
    let oldest = (now - chrono::Duration::from_std(since).unwrap_or_default()).date_naive();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(d) = parse(name)
            && d >= oldest
            && e.file_type().is_ok_and(|t| t.is_file())
        {
            out.push((d, e.path(), e.metadata().map_or(0, |m| m.len())));
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    out
}

/// 讀取檔案的最後 `max` bytes（截斷時從下一個換行開始，避免半行）；回傳 (內容, 是否截斷)
pub fn read_tail(path: &Path, max: u64) -> std::io::Result<(String, bool)> {
    use std::io::{Seek, SeekFrom};
    // 安全稽核 N14：不跟隨符號連結——診斷命令可能以 root 執行（sudo pulse diag／kairos diag），
    // 這些來源路徑可能位於服務帳號可寫的目錄；跟隨連結會把 /etc/shadow 等 root 檔案讀進診斷包。
    // 來源是符號連結時 open 失敗，呼叫端略過該檔
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = opts.open(path)?;
    let len = f.metadata()?.len();
    let truncated = len > max;
    if truncated {
        f.seek(SeekFrom::Start(len - max))?;
    }
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf)?;
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if truncated && let Some(i) = s.find('\n') {
        s = s[i + 1..].to_string();
    }
    Ok((s, truncated))
}

/// 檔案清單（路徑、大小、權限、擁有者、修改時間；不含內容）。目錄另列出其中最多 500 個項目
pub fn file_listing(paths: &[PathBuf]) -> String {
    let mut out = String::from("# 路徑\t大小\t權限\t修改時間（UTC）\n");
    let mut line = |p: &Path| {
        let Ok(m) = std::fs::symlink_metadata(p) else {
            out.push_str(&format!("{}\t（不存在或無法讀取）\n", p.display()));
            return;
        };
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            format!(
                "{:04o} uid={} gid={}",
                m.permissions().mode() & 0o7777,
                m.uid(),
                m.gid()
            )
        };
        #[cfg(not(unix))]
        let mode = if m.permissions().readonly() {
            "readonly"
        } else {
            "rw"
        }
        .to_string();
        let mtime = m
            .modified()
            .ok()
            .map(|t| {
                DateTime::<Utc>::from(t)
                    .format("%Y-%m-%dT%H:%M:%SZ")
                    .to_string()
            })
            .unwrap_or_default();
        out.push_str(&format!(
            "{}\t{}\t{mode}\t{mtime}{}\n",
            p.display(),
            m.len(),
            if m.is_dir() { "\t目錄" } else { "" }
        ));
    };
    for p in paths {
        line(p);
        if p.is_dir()
            && let Ok(entries) = std::fs::read_dir(p)
        {
            let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            names.sort();
            for n in names.into_iter().take(500) {
                line(&n);
            }
        }
    }
    out
}

/// 執行外部命令並取得輸出（最多 `timeout`；不存在或失敗時回說明文字）
pub fn command_output(program: &str, args: &[&str], timeout: Duration) -> String {
    use std::process::{Command, Stdio};
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return format!("（無法執行 {program}：{e}）\n"),
    };
    // 在另一個執行緒讀輸出，避免輸出很多時填滿管線而卡住
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_string(&mut out);
        }
        if let Some(mut s) = stderr {
            let mut err = String::new();
            let _ = s.read_to_string(&mut err);
            out.push_str(&err);
        }
        out
    });
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return format!("（{program} 超過 {} 秒未完成）\n", timeout.as_secs());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return format!("（{program}：{e}）\n"),
        }
    }
    reader.join().unwrap_or_default()
}

/// 以 0600 建立新檔（已存在時不覆寫），寫入並同步到磁碟
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

/// 期間的中文表示（`3 天`、`24 小時`）
pub fn humanize(d: Duration) -> String {
    let s = d.as_secs();
    if s.is_multiple_of(86_400) {
        format!("{} 天", s / 86_400)
    } else if s.is_multiple_of(3600) {
        format!("{} 小時", s / 3600)
    } else {
        format!("{} 分鐘", s.div_ceil(60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kp-obs-bundle-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 安全稽核 N14：read_tail 不跟隨符號連結（診斷命令可能以 root 執行，來源可能在服務帳號可寫的
    /// 目錄，跟隨連結會把 root 檔案讀進診斷包）
    #[cfg(unix)]
    #[test]
    fn read_tail_does_not_follow_symlinks() {
        let dir = temp("nofollow");
        let real = dir.join("secret");
        std::fs::write(&real, "機密內容\n第二行\n").unwrap();
        // 一般檔案照常讀得到
        let (text, _) = read_tail(&real, 1024).unwrap();
        assert!(text.contains("機密內容"), "{text}");
        // 指向它的符號連結 → 以 O_NOFOLLOW 開啟失敗（Linux 為 ELOOP），診斷包不會把目標讀進來
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = read_tail(&link, 1024).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ELOOP), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn logs_are_collected_newest_first_within_the_limit() {
        let dir = temp("logs");
        let day = |d: u32| NaiveDate::from_ymd_opt(2026, 10, d).unwrap();
        for d in [10u32, 11, 12, 13] {
            std::fs::write(
                dir.join(format!("x-{}.log", day(d))),
                format!("第一行 {d}\n{}\n", "y".repeat(100)),
            )
            .unwrap();
        }
        std::fs::write(dir.join("other.txt"), "z").unwrap();
        let parse = |n: &str| {
            n.strip_prefix("x-")?
                .strip_suffix(".log")?
                .parse::<NaiveDate>()
                .ok()
        };
        let now = day(13).and_hms_opt(12, 0, 0).unwrap().and_utc();
        let files = log_files(&dir, parse, Duration::from_secs(2 * 86_400), now);
        assert_eq!(
            files.iter().map(|f| f.0).collect::<Vec<_>>(),
            vec![day(13), day(12), day(11)]
        );
        let mut b = Bundle::new("t", 0, None);
        let (n, bytes) = b.logs(&files, 150);
        assert!(b.truncated);
        assert_eq!(n, 2);
        assert!(bytes <= 150);
        let names: Vec<_> = b.tar.files().iter().map(|(p, _)| p.to_string()).collect();
        assert_eq!(
            names,
            vec!["t/logs/x-2026-10-13.log", "t/logs/x-2026-10-12.log"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finish_refuses_leaks_and_writes_private_files() {
        let mut b = Bundle::new("t", 0, None);
        b.text("a.txt", "password = \"x\"\n");
        let gz = b
            .finish(
                false,
                |tar, b| serde_json::json!({"n": tar.len(), "removed": b.removed_json()}),
            )
            .unwrap();
        let tar = crate::archive::gunzip(&gz).unwrap();
        let text = String::from_utf8_lossy(&tar);
        assert!(text.contains("t/MANIFEST.json") && text.contains("t/REDACTION.txt"));
        assert!(text.contains("secret_value\t1"));
        // 不經過遮蔽直接放入的秘密：拒絕產生
        let mut b = Bundle::new("t", 0, None);
        let _ = b.tar.add(
            "t/raw.txt",
            "-----BEGIN PRIVATE KEY-----\nx\n-----END PRIVATE KEY-----\n",
            0o600,
        );
        assert!(b.finish(false, |_, _| serde_json::json!({})).is_err());

        let dir = temp("write");
        let p = dir.join("o.tar.gz");
        write_private(&p, b"x").unwrap();
        assert!(write_private(&p, b"y").is_err(), "不覆寫既有的檔案");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(humanize(Duration::from_secs(3 * 86_400)), "3 天");
        assert_eq!(humanize(Duration::from_secs(7200)), "2 小時");
    }

    #[cfg(unix)]
    #[test]
    fn command_output_and_timeout() {
        assert!(
            command_output(
                "sh",
                &["-c", "echo hi; echo err >&2"],
                Duration::from_secs(5)
            )
            .contains("hi")
        );
        assert!(command_output("sleep", &["5"], Duration::from_millis(200)).contains("超過"));
        assert!(
            command_output("/nonexistent-cmd", &[], Duration::from_secs(1)).contains("無法執行")
        );
    }
}
