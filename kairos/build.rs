//! 決定嵌入執行檔的 commit（`kairos --version` 與啟動日誌第一行；D3）：
//! 1. 環境變數 `BUILD_COMMIT`（發佈建置時指定；去除前後空白後不得為空）；
//! 2. 否則 `git rev-parse --short HEAD`；
//! 3. 都失敗時為 `unknown`。
//!
//! 結果以 `KAIROS_BUILD_COMMIT` 編譯期環境變數提供給程式。

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=BUILD_COMMIT");
    println!("cargo:rerun-if-changed=build.rs");

    let commit = std::env::var("BUILD_COMMIT")
        .ok()
        .and_then(|v| sanitize(&v))
        .or_else(git_commit)
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=KAIROS_BUILD_COMMIT={commit}");
}

/// 去除前後空白並拒絕控制字元（會破壞 cargo 指令與版本字串）；結果為空時回傳 `None`
fn sanitize(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    Some(value.to_string())
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .and_then(|s| sanitize(&s))
}

/// 以 git 取得目前的短 commit，並在 HEAD 或分支移動時重新執行本腳本
fn git_commit() -> Option<String> {
    let commit = git(&["rev-parse", "--short", "HEAD"])?;
    // HEAD 本身、HEAD 指向的分支 ref 與 packed-refs：任一變化都代表 commit 可能不同
    let mut watched = vec!["HEAD".to_string(), "packed-refs".to_string()];
    if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watched.push(head_ref);
    }
    for name in watched {
        if let Some(path) = git(&["rev-parse", "--git-path", &name])
            && Path::new(&path).exists()
        {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    Some(commit)
}
