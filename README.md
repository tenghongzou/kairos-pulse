# kairos-pulse

Kairos（TLS/mTLS 監控伺服器）與 Pulse（監控代理）的 monorepo，以單一 Cargo workspace 管理。

## 目錄結構

```
kairos-pulse/
├── Cargo.toml             workspace 設定（members = kairos、pulse、crates/*）
├── Cargo.lock             整個 workspace 共用的依賴版本鎖定檔
├── rust-toolchain.toml    固定 Rust 1.99.0（含 rustfmt、clippy）
├── deny.toml              cargo-deny 設定（安全公告、授權、重複版本、來源）
├── .github/workflows/     CI：Linux／Windows 的 fmt、clippy、test 與 cargo deny
├── crates/                兩個產品共用的內部 crate（publish = false）
│   ├── kairos-proto/      協定型別、序列化與欄位驗證；PROTOCOL.md 為唯一的協定規格
│   └── date-guard/        依日期命名檔案的保留清除規則與時鐘跳躍／回撥保護（DateGuard）
├── kairos/                Kairos 伺服器（說明見 kairos/README.md）
└── pulse/                 Pulse 代理（說明見 pulse/README.md）
```

協定規格只有一份：`crates/kairos-proto/PROTOCOL.md`。`kairos-proto` 的單元測試以 `include_str!` 讀入該文件，直接從「範例」節解析測試向量、從「錯誤碼」表比對 fatal 預設值；`kairos/PROTOCOL.md`、`pulse/PROTOCOL.md` 只是指向它的說明，發佈包內附的則是完整規格。`crates/` 下的共用 crate 都是 `publish = false` 的內部 crate，只使用 workspace 既有的依賴，不做 I/O 也不記日誌（狀態檔讀寫、NDJSON codec 與日誌措辭留在各產品）。

日誌節流沒有共用：Kairos 的 `throttle.rs` 依「類別／類別 + 來源 IP／類別 + agent_id」分別計數，另有每類別的總量上限與被略過 agent 的列舉，可在多執行緒間共用（`Mutex`）；Pulse 的 `log_throttle.rs` 是單一計時器（搭配 `AttentionLog` 在類別改變時重新起算），以 tokio 的時鐘計時以便在暫停時鐘下測試。兩者只有「時間窗內略過並計數」這幾行相同，抽成共用 crate 反而要為時鐘型別與計數語意加上泛型，因此維持各自實作。

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
