//! 建置腳本：決定 `pulse --version` 顯示的 commit，透過環境變數 `PULSE_BUILD_COMMIT` 交給程式。
//!
//! 來源優先順序：
//! 1. 環境變數 `BUILD_COMMIT`（CI / 打包腳本指定，例如 `BUILD_COMMIT=b882494 cargo build --release`）
//! 2. `git rev-parse --short HEAD`（在 git 工作目錄內建置時）
//! 3. 都取不到時為 `unknown`
//!
//! 只保留 ASCII 英數字與 `.`、`-`、`_`、`+`，最長 64 字元，避免把控制字元等帶進版本字串與日誌。

use std::path::{Path, PathBuf};
use std::process::Command;

const MAX_COMMIT_LEN: usize = 64;

fn sanitize(raw: &str) -> Option<String> {
    let s: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
        .take(MAX_COMMIT_LEN)
        .collect();
    (!s.is_empty()).then_some(s)
}

fn git(manifest_dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout)
        .ok()
        .map(|s| s.trim().to_string())
}

/// commit 改變時重新執行：監看 HEAD、它指向的 ref 檔與 packed-refs（只監看存在的檔案，
/// 不存在的路徑會讓 cargo 每次都重跑建置腳本）。
fn watch_git_head(manifest_dir: &Path) {
    // 找不到 git 指令時仍監看 <套件目錄>/.git，之後裝好 git 或切換 commit 時會重新取得
    let git_dir = git(manifest_dir, &["rev-parse", "--git-dir"]).unwrap_or_else(|| ".git".into());
    let git_dir = {
        let p = PathBuf::from(git_dir);
        if p.is_absolute() {
            p
        } else {
            manifest_dir.join(p)
        }
    };
    let head = git_dir.join("HEAD");
    let mut watch = vec![head.clone(), git_dir.join("packed-refs")];
    if let Ok(content) = std::fs::read_to_string(&head)
        && let Some(r) = content.trim().strip_prefix("ref: ")
    {
        watch.push(git_dir.join(r));
    }
    for path in watch.into_iter().filter(|p| p.is_file()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=BUILD_COMMIT");

    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let commit = match std::env::var("BUILD_COMMIT")
        .ok()
        .as_deref()
        .and_then(sanitize)
    {
        Some(c) => c,
        None => {
            watch_git_head(&manifest_dir);
            git(&manifest_dir, &["rev-parse", "--short", "HEAD"])
                .as_deref()
                .and_then(sanitize)
                .unwrap_or_else(|| "unknown".to_string())
        }
    };
    println!("cargo:rustc-env=PULSE_BUILD_COMMIT={commit}");
}
