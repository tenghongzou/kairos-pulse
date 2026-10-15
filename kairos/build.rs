//! 決定嵌入執行檔的 commit（`kairos --version` 與啟動日誌第一行；D3）：
//! 1. 環境變數 `BUILD_COMMIT`（發佈建置時指定；去除前後空白後不得為空）；
//! 2. 否則 `git rev-parse --short HEAD`；
//! 3. 都失敗時為 `unknown`。
//!
//! 結果以 `KAIROS_BUILD_COMMIT` 編譯期環境變數提供給程式。
//!
//! 另外產生主控台網頁介面（`web/index.html` 與 `web/assets/` 下的所有檔案）的清單
//! `$OUT_DIR/web_assets.rs`，以 `include_bytes!` 嵌入執行檔（`src/api/web.rs`）。清單依路徑排序、
//! 內容只取決於檔案本身，發佈包的可重現性不受影響；`web/tests/`、`web/tools/`、`web/README.md`
//! 不嵌入。

use std::path::{Path, PathBuf};
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

    web_assets();
}

/// 嵌入的網頁檔：`(相對於 web/ 的路徑, 檔案)`，依路徑排序
fn web_files(web: &Path) -> Vec<(String, PathBuf)> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // 隱藏檔（編輯器的暫存檔等）不嵌入
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let rel = format!("{prefix}{name}");
            if path.is_dir() {
                walk(&path, &format!("{rel}/"), out);
            } else {
                out.push((rel, path));
            }
        }
    }
    let mut files = Vec::new();
    let index = web.join("index.html");
    if index.is_file() {
        files.push(("index.html".to_string(), index));
    }
    walk(&web.join("assets"), "assets/", &mut files);
    files.sort();
    files
}

fn web_assets() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let web = manifest.join("web");
    // 目錄：cargo 會掃描其中所有檔案的變更
    println!("cargo:rerun-if-changed=web/index.html");
    println!("cargo:rerun-if-changed=web/assets");
    let mut code = String::from(
        "// 由 build.rs 產生：主控台網頁介面的檔案（相對於 kairos/web/ 的路徑, 內容）\n\
         pub static FILES: &[(&str, &[u8])] = &[\n",
    );
    for (rel, path) in web_files(&web) {
        // CARGO_MANIFEST_DIR 是絕對路徑，path 也是
        code.push_str(&format!(
            "    ({rel:?}, include_bytes!({:?})),\n",
            path.to_string_lossy()
        ));
    }
    code.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default()).join("web_assets.rs");
    // 內容相同時不改寫（避免不必要的重新編譯）
    if std::fs::read_to_string(&out).ok().as_deref() != Some(code.as_str()) {
        std::fs::write(&out, code).expect("無法寫入 web_assets.rs");
    }
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
