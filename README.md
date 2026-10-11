# kairos-pulse

Kairos（TLS/mTLS 監控伺服器）與 Pulse（監控代理）的 monorepo，以單一 Cargo workspace 管理。

## 目錄結構

```
kairos-pulse/
├── Cargo.toml             workspace 設定（members = kairos、pulse）
├── Cargo.lock             整個 workspace 共用的依賴版本鎖定檔
├── rust-toolchain.toml    固定 Rust 1.99.0（含 rustfmt、clippy）
├── deny.toml              cargo-deny 設定（安全公告、授權、重複版本、來源）
├── .github/workflows/     CI：Linux／Windows 的 fmt、clippy、test 與 cargo deny
├── kairos/                Kairos 伺服器（說明見 kairos/README.md）
└── pulse/                 Pulse 代理（說明見 pulse/README.md）
```

兩個產品共用的協定規格目前各有一份（`kairos/PROTOCOL.md`、`pulse/PROTOCOL.md`，內容相同）。

| 產品 | 說明 | 安裝與打包 |
|---|---|---|
| `kairos/` | 接受 Pulse 以 TLS（可選 mTLS）長連線上報的監控伺服器 | `kairos/packaging/INSTALL.md` |
| `pulse/` | 收集系統資訊並上報給 Kairos 的代理 | `pulse/packaging/INSTALL.md` |

## 建置與測試

在 monorepo 根目錄執行（`rust-toolchain.toml` 會讓 rustup 自動使用 1.99.0）：

```bash
cargo build --workspace                 # 建置兩個產品；輸出在根目錄的 target/
cargo build -p kairos --release         # 只建置其中一個
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check                        # 需先 cargo install cargo-deny --locked
```

在 `kairos/` 或 `pulse/` 目錄內執行 `cargo build`、`cargo test`、`cargo run` 時只會處理該產品，各自 README 中的指令都以產品目錄為工作目錄。

發佈包（Linux x86_64 musl 靜態執行檔，需要 Docker）：

```bash
sh kairos/packaging/build-release.sh    # 輸出到 kairos/dist/
sh pulse/packaging/build-release.sh     # 輸出到 pulse/dist/
```

`--version` 顯示的 commit 是本 monorepo 的 commit。

## 歷史來源

本 repo 由兩個獨立 repo 合併而成，保留各自的完整歷史：

- `kairos/`：來自 Kairos repo 的 `master`（`49ba691`，共 10 個 commit）
- `pulse/`：來自 pulse repo 的 `master`（`12e6c69`，共 15 個 commit）

匯入方式：先以 `git filter-branch --index-filter` 把每個 commit 的檔案移到 `kairos/`、`pulse/` 子目錄，再以 `git merge --allow-unrelated-histories` 合併。改寫只移動路徑，作者、日期、commit 訊息與檔案內容都不變，因此可以直接用 `git log -- kairos/src/main.rs`、`git blame` 追溯原始歷史；但 commit hash 會改變。已部署執行檔 `--version` 中的舊 commit 可用下表對照。

<details>
<summary>原始 commit 與本 repo 中 commit 的對照</summary>

Kairos：

| 原始 | 本 repo | 說明 |
|---|---|---|
| `49ba691` | `692a339` | INSTALL.md 註明確認清單需以 root 執行 |
| `bbf4ce1` | `717fe1f` | 修正地端部署測試發現的問題並新增發佈打包 |
| `4223101` | `551950d` | 修復第二輪外部審計發現的問題 |
| `572b955` | `2eb463d` | 修復外部審計發現的資安與正確性問題 |
| `a89803e` | `e53605b` | Implement Pulse agent protocol with mTLS, storage and graceful shutdown |
| `d5b6c7d` | `b9623ec` | Ignore TLS certificate and key files |
| `13021af` | `8d6aa56` | Add README |
| `badf326` | `981bc57` | Upgrade dependencies to tokio-rustls 0.26 / rustls 0.23 |
| `4d90ed1` | `25eb195` | Fix TLS server robustness issues |
| `735e717` | `e1f19db` | init project kairos |

Pulse：

| 原始 | 本 repo | 說明 |
|---|---|---|
| `12e6c69` | `4a1a5b4` | docs: INSTALL 多實例憑證目錄分開、移除時刪除 CSR、補充狀態文字的監控方式 |
| `a67c715` | `d80df11` | fix: 修正 systemd 安裝驗證發現的問題 |
| `47ebd57` | `acd3fff` | fix: 修正地端部署測試發現的問題並新增發佈打包 |
| `b882494` | `35008ac` | fix: 修復第二輪外部審計發現的問題 |
| `44dcee5` | `a8fb5c6` | fix: 修復外部審計發現的資安與正確性問題 |
| `081f859` | `69bf730` | docs: README 改以錯誤碼說明身分驗證失敗 |
| `1cf6c89` | `b30fdcb` | feat: 改為 NDJSON 長連線協定並支援 mTLS |
| `6df00e5` | `cd26ad9` | fix: 修正系統資訊正確性、TLS 安全性並補文件 |
| `4006a22` | `5bcd3aa` | feat: Arc share |
| `d59ed4a` | `b5a1cf0` | feat: get_real_system_info |
| `ef2323a` | `5c62c33` | feat: update config init local |
| `0bc2656` | `0b3df4c` | feat: add log path |
| `a156a5f` | `eddef93` | feat: update config to file |
| `2210f49` | `8c80595` | feat: add get system_info and hearbeat and network send test |
| `140db47` | `761f117` | init project Pulse |

</details>
