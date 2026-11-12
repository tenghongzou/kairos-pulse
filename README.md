# kairos-pulse

Kairos（TLS/mTLS 監控伺服器）與 Pulse（監控代理）的 monorepo，以單一 Cargo workspace 管理。

## 目錄結構

```
kairos-pulse/
├── Cargo.toml             workspace 設定（members = kairos、pulse、crates/*、tests/e2e）
├── Cargo.lock             整個 workspace 共用的依賴版本鎖定檔
├── rust-toolchain.toml    固定 Rust 1.99.0（含 rustfmt、clippy）
├── deny.toml              cargo-deny 設定（安全公告、授權、重複版本、來源）
├── CHANGELOG.md           變更紀錄（Kairos／Pulse／共用；含破壞性變更與升級步驟，也放進兩個發佈包）
├── RELEASING.md           發佈流程：版本規則、打 tag、建置、簽章與發佈金鑰管理
├── .github/workflows/     CI：Linux／Windows 的 fmt、clippy、test（含端對端測試）與 cargo deny，全部通過後打包發佈包
├── packaging/             兩個產品共用的打包實作、release.sh、簽章、可重現性檢查與 cargo-about 設定
├── crates/                兩個產品共用的內部 crate（publish = false）
│   ├── kairos-proto/      協定型別、序列化與欄位驗證；PROTOCOL.md 為唯一的協定規格
│   ├── date-guard/        依日期命名檔案的保留清除規則與時鐘跳躍／回撥保護（DateGuard）
│   └── kp-obs/            可觀測性：日誌格式（text／json／legacy）、syslog（RFC 5424）、Prometheus 文字格式、
│                          診斷包（tar.gz、敏感資料遮蔽）、--check-config 的輸出
├── kairos/                Kairos 伺服器（說明見 kairos/README.md）
├── pulse/                 Pulse 代理（說明見 pulse/README.md）
└── tests/e2e/             端對端測試 kp-e2e（publish = false）：啟動實際的 kairos 與 pulse 執行檔
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
cargo test --workspace                  # 含端對端測試（見下）
cargo test -p kp-e2e                    # 只跑端對端測試
cargo deny check                        # 需先 cargo install cargo-deny --locked
```

在 `kairos/` 或 `pulse/` 目錄內執行 `cargo build`、`cargo test`、`cargo run` 時只會處理該產品，各自 README 中的指令都以產品目錄為工作目錄。

### 端對端測試（`tests/e2e`）

`kp-e2e` 以**實際編譯出的** `kairos`、`pulse` 執行檔在 `127.0.0.1` 上互連（審計 Q1），不經過任何外部網路：

| 測試檔 | 情境 |
|---|---|
| `tests/session.rs` | mTLS 註冊（兩端日誌都寫出協商為協定 v2）→ heartbeat 收到 ack → report 寫入 SQLite（以實際的 `kairos reports export` 取出；`schema` 2 的通用指標，逐項檢查標準指標的名稱、單位與值），JSONL 鏡像與資料庫一致 → 兩端優雅關機（結束代碼 0、`close_notify`、最後的 `agents.json` 標記離線，並有 `proto` 2 與 hello 的主機資訊；`kairos hosts list --json` 的主機清冊中為離線、帶相同的主機資訊）；CRL 熱重載撤銷現役連線（Kairos 踢線、之後的握手被拒；Pulse 的日誌、stderr 與 Linux 上 sd_notify 的 `Status:`）；Kairos 停止後在同一連接埠重新啟動，從主機清冊載回主機，Pulse 自動重新註冊並繼續送 report |
| `tests/compat.rs` | 協定 v1 相容：以假 client（`RawClient`，送出與舊版 0.1.0 Pulse 相同的 hello、heartbeat、report）與新版 Pulse 同時連同一個新版 Kairos。舊版得到與 v1 Kairos 相同的 `hello_ack`（`proto` 1，沒有 v2 欄位）與錯誤處理，匯出的 report 為 `schema` 1 且欄位與舊版的 JSONL 相同，新版為 `schema` 2，兩種格式也寫在同一個 JSONL 鏡像檔；`agents.json` 中 v1 的 agent 沒有主機資訊。新版 Pulse 沒有只用 v1 的設定，新版 Pulse 連舊版 Kairos 的方向由 Pulse 的單元測試涵蓋 |
| `tests/durable.rs` | durable（at-least-once，A5）：Pulse 持續送資料時讓 Kairos 停機再啟動，停機期間的取樣補送後在 SQLite 中不缺號、不重複，收到時間（`recv_at`）比取樣時間（`ts`）晚；Kairos 已寫入、client 沒收到 `ack` 就斷線（結果不明）時重送只保存一次，Kairos 重新啟動後的重送也一樣（唯一索引） |
| `tests/exit_codes.rs` | 兩端常駐模式啟動時的結束代碼（0、1、2、3、4、11，見下方「結束代碼」） |
| `tests/api.rs` | 主控台 API（`kairos/API.md`）：以實際的 `kairos api-token create` 建立 token（Kairos 在日誌中記錄建立、不含秘密），先開 SSE 再啟動 Pulse → `host.added`；主機清單（狀態、CIDR 搜尋、計數）、詳情（durable）、CPU 趨勢、總覽、系統健康；Pulse 停止 → `host.offline`（`connection_closed`），以 `Last-Event-ID` 重連補送；CRL 撤銷 → `host.offline`（`certificate_rejected`），之後握手被拒的憑證歸屬到主機（`KRS-W1105`） |
| `tests/enroll.rs` | 註冊通道 8444（Kairos 端，測試用的 HTTPS 用戶端以 `kairos-proto` 的型別組請求）：申請 → CLI 核准 → 長輪詢取回 → 以該憑證連 8443（`hello_ack` 宣告 `renew`／`deregister`）；8443 的正常輪替與 `superseded`；撤銷後 5 秒內被斷線；除役與 10 分鐘視窗；過期續簽、重放在 Kairos 重新啟動後仍被拒、時鐘偏差；寬限期內成功、超過後 `renew_grace_exceeded`、允許再次自動續簽後成功（#14，0.9.0）；手動簽發憑證的匯入與撤銷後 hello 被拒（0.9.0）；下載端點（0.9.0）；模式切換、`config show --effective`、限流 |
| `tests/enroll_pulse.rs` | 註冊通道的完整流程（實際的 `pulse enroll`、Pulse 服務、`pulse decommission`）：安裝字串自動核准、人工核准由背景服務取回、token 錯字／無效／過期／用完／停用、CA 指紋不符、拒絕、撤回、離線簽發（匯出 → `sign-bundle` → 匯入）、回應遺失後重送、限流與僅續簽、過期寬限期續簽（含回應遺失）、撤銷後斷線與 `--force` 重新註冊、8443 輪替、除役、沒有 8444 的 Kairos；並檢查 `identity/` 權限與秘密不外洩（ENROLL.md 第 19.5 節） |
| `tests/users.rs` | 使用者與稽核（0.6.0）：沒有預設帳號，首位管理員以實際的 `kairos users create --role admin` 建立 → 以一次性密碼登入（必須先變更密碼；cookie 的 HttpOnly、Secure、SameSite=Strict 與 CSRF）→ 建立其他使用者；`kairos audit verify` 驗證雜湊鏈，啟動、登入、密碼變更、關機都在鏈中，日誌與輸出不含密碼 |
| `tests/alerts.rs` | 三條線的整合（0.6.0，兩端的日誌都是 `log.format = "json"`）：首位管理員 → 建立維運者與唯讀 → 管理員新增 Webhook 管道（測試內的本機 HTTP 接收器；維運者 403）並綁到主機離線規則 → Pulse 停止 → Webhook 收到簽章的 `kairos.alert.v1` → 唯讀確認 403、沒有 CSRF token 403 → 維運者以工作階段確認 → `/metrics` 有告警、通知與主機的計數 → `kairos audit verify` 通過，告警操作與被拒的請求在鏈中；兩端的每一行輸出都是 v1 的 JSON 日誌，日誌不含機密與密碼 |
| `tests/import.rs` | `kairos reports import`：Kairos 執行中拒絕（檔案鎖跨行程；主控台的寫入連線不得放掉這把鎖）、停止後匯入、重新啟動後匯出與清冊 |
| `tests/export.rs` | 指標轉出（0.9.0，`kairos/EXPORT.md`）：`[export.prometheus]`、`[export.otlp]` 啟用的 Kairos 與 Pulse → 連線保存的 report 出現在 `GET /api/v1/export/prometheus`（`kairos api-token create` 的唯讀 token）→ OTLP/HTTP 推送到測試內的假 collector（`headers_file` 的標頭、semconv 名稱、`resource_attributes`）→ Pulse 結束後 `kairos_host_up` 為 0、不再輸出指標 → `/metrics` 有推送的計數 → 正常關機、日誌沒有 OTLP 錯誤 |
| `tests/privacy.rs` | 個資保護（0.9.0，`kairos/PRIVACY.md`）：`[privacy]` 的假名化與網段（清冊、`agents.json`、JSONL 鏡像；`kairos hosts pseudonym`；`privacy.key` 為 0600）、仍連線中不能刪除、主控台 API 的個資匯出與刪除主機資料、`kairos hosts purge` 在 Kairos 執行中拒絕（`KRS-E4304`）與停止後刪除 |
| `tests/remote.rs`、`tests/releases_cli.rs` | 集中設定與 Pulse 遠端升級（0.9.0／0.10.0，`crates/kairos-proto/REMOTE.md`）：以送出相同訊息的模擬 client 走完設定下發、版本匯入、維護時段與已排程的推送、分段下載、升級中的離線告警抑制到推送完成；`kairos releases import\|list\|delete` 與執行中的 Kairos 並用、版本庫檔案遺失的系統健康（`KRS-W8417`〔暫定〕）、外部 URL 的版本庫 |
| `tests/postgres.rs` | PostgreSQL 儲存後端（0.10.0，`kairos/STORAGE.md` 第 19 節；需要 `KAIROS_TEST_PG_URL`，沒有設定時明確略過、`KAIROS_TEST_PG_REQUIRED=1` 時不可略過）：實際的 Kairos＋Pulse 跑在 PostgreSQL 上，補送不重複不缺號、重新啟動後載回主機、稽核鏈驗證、`kairos db migrate` 搬回 SQLite 後內容相同 |
| `tests/cluster.rs` | 叢集模式（0.10.0，階段 A：主動／備援，`kairos/HA.md` 第 17.1 節；需要 PostgreSQL，只在 Linux）：兩個 Kairos＋Pulse 的容錯移轉（SIGKILL、`step-down`）後 durable 的資料不缺號不重複、凍結的舊 leader 被 fencing 擋下並以代碼 75 結束、排空、啟動保護；遠端管理在換手後的狀態（`remote_upgrade_state_survives_failover`：版本庫是節點本機的檔案、已排程的推送由接手的節點繼續、替換中主機的離線告警抑制從資料庫重建）。`database_outage_makes_nodes_wait_and_recover` 預設不執行（會停止測試用的資料庫容器） |

- 第一次需要執行檔時，測試以 `cargo build -p kairos -p pulse` 建置到與測試相同的 target 目錄與 profile（已是最新時幾乎不花時間），所以測的一定是目前的原始碼；設定環境變數 `KP_E2E_BIN_DIR=<目錄>` 時改用該目錄中已建置好的執行檔、不建置。
- 每個測試使用自己的暫存目錄（`<系統暫存目錄>/kp-e2e-*`）與隨機埠，可平行執行；每個等待都有逾時（30 秒）。測試失敗時保留暫存目錄（路徑印在輸出中）並附上兩端的 stdout／stderr。
- 測試用的設定縮短了時間：心跳與 report 每 1 秒、重連延遲 1～2 秒（A11 的啟動隨機延遲因此最多 1 秒）、Kairos 熱重載每 1 秒。Kairos 使用預設的 SQLite 並開啟 JSONL 鏡像（`storage.jsonl_mirror = true`）。註冊通道的測試另用只在 debug 建置生效的測試掛鉤縮短憑證效期、模擬回應遺失、縮短過期續簽的等待（`KAIROS_TEST_*`、`PULSE_TEST_*`；子行程不繼承環境中的這些變數）。
- 憑證：Kairos 的伺服器憑證用 `pulse/testdata/server.pem`（Pulse 與主控台 API 的用戶端 `Api` 以 `server-ca.pem` 驗證；主控台在另一個隨機埠）；客戶端憑證、私鑰、client CA 與 CRL 沿用 Kairos 整合測試的 `kairos/src/test_pki.rs`（以 `#[path]` 引用同一個檔案，`kairos/testdata/pki` 沒有客戶端私鑰）。`RawClient` 以 workspace 已使用的 `rustls`（ring）建立與 Pulse 相同的 mTLS 連線，不新增外部套件。
- 結束訊號：Unix 送 SIGTERM；Windows 以 `CREATE_NEW_PROCESS_GROUP` 啟動子行程後送 Ctrl+Break（兩個程式都會攔截並正常關機）。
- 整套約 20 秒（不含第一次建置，各檔平行執行）；沒有標記 `#[ignore]` 的慢速測試。

## 結束代碼（兩個產品共用）

依設計包 `design/design-system/cli-style.md` §8（Pulse 另見 `design/pulse/cli-spec.md` §1.3）。常駐模式（`kairos`、`pulse` 不帶子命令）啟動與結束時使用：

| 代碼 | 意義 | Kairos | Pulse | systemd 重試 |
|---|---|---|---|---|
| 0 | 正常結束 | 收到關機訊號後正常關機；`--help`、`--version` | 同左 | 否 |
| 1 | 一般錯誤 | 背景工作異常、看門狗強制結束；資料或日誌目錄無法建立等未分類的啟動失敗 | 收集任務異常；日誌目錄無法建立、無法取得主機名稱等；`--print-agent-id` 失敗 | 是 |
| 2 | 用法錯誤 | 無法辨識的參數、`--config` 缺少路徑或重複指定 | 未知的參數 | **否** |
| 3 | 設定錯誤 | 設定檔語法、型別、範圍錯誤；`--config` 指定的檔案不存在；未啟用 mTLS 卻監聽非 loopback 位址（S1） | `pulse.toml`／`PULSE__*` 的語法、型別、範圍錯誤；`log.level` 無效；`server.domain` 不是有效的名稱或 IP；`use_system_roots = false` 卻未設定 `ca_cert` | **否** |
| 4 | 憑證錯誤 | 伺服器憑證、私鑰、`auth.client_ca`、`auth.crl` 無法讀取、格式錯誤或不成對 | `server.ca_cert`、客戶端憑證或私鑰無法讀取、格式錯誤、不成對；SAN 與 `agent_id` 不符；`agent.id` 無法讀取、內容不合法或無法建立 | **否** |
| 11 | 衝突 | `server.listen` 的連接埠已被佔用 | （不使用） | 是（舊的行程結束後可能就能啟動） |
| 130 | 強制結束 | 關機期間再收到一次訊號 | 同左 | 是 |

`kairos.service`、`pulse.service` 以 `RestartPreventExitStatus=2 3 4` 讓 systemd 不再重試「重試也不會成功」的錯誤：服務停在 `failed`（`systemctl status` 顯示 `status=2`／`3`／`4`），修正後執行 `systemctl restart`。只判斷「非 0」的腳本不受影響。

發佈檔（需要 Docker；0.8.0 起：Linux x86_64／aarch64 的 tar.gz、deb、rpm，Pulse 另有 Windows x86_64 的 zip、msi 與一行安裝腳本）：

```bash
sh kairos/packaging/build-release.sh    # dist/kairos-<版本>-<commit>-linux-{x86_64,aarch64}.{tar.gz,deb,rpm,cdx.json}
sh pulse/packaging/build-release.sh     # dist/pulse-…-linux-{x86_64,aarch64}.{tar.gz,deb,rpm,cdx.json}、…-windows-x86_64.{zip,msi,cdx.json}、pulse-install.sh
TARGETS=x86_64-unknown-linux-musl PACKAGES= sh kairos/packaging/build-release.sh   # 只建 x86_64 的 tar.gz（開發用）
```

兩個腳本都輸出到根目錄的 `dist/`，並更新同一份合併的 `dist/SHA256SUMS`（只替換本次產出檔案的那幾列）。共用實作在 `packaging/build-release-common.sh`：建置 image 以 `@sha256:` digest 固定，來源取自 `git archive`（與工作目錄的行尾設定無關），發佈包內附 `THIRD-PARTY-LICENSES` 與 SBOM。正式發佈（檢查、打包、簽章）請用 `sh packaging/release.sh`，流程見 [RELEASING.md](RELEASING.md)；`sh packaging/check-reproducible.sh` 檢查建置是否可重現。

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
