# Kairos 高可用（HA）規格（商業審計 A2；衝刺 8 規格草稿）

本文件是 Kairos 與 Pulse「高可用」的工程規格，涵蓋以下內容：

- 部署拓撲與分階段計畫
- 外部資料庫（PostgreSQL）後端與儲存抽象
- 叢集協調：節點、租約、leader 與 fencing
- report 寫入、去重與彙總在多節點下的語意
- Pulse 的多伺服器位址與 DNS SRV
- 主控台、PKI、機密與稽核雜湊鏈在多節點下的處理
- 升級路徑與故障情境
- 暫定錯誤代碼、實作分工與已決定事項（第 21 節）

協定的線上格式仍以 [`crates/kairos-proto/PROTOCOL.md`](../crates/kairos-proto/PROTOCOL.md) 為準；本文件第 18 節的協定增補是草稿，實作時再併入 PROTOCOL.md。儲存的現況見 [`STORAGE.md`](STORAGE.md)，PKI 見 [`PKI.md`](PKI.md)，主控台 API 見 [`API.md`](API.md)，告警見 [`ALERTS.md`](ALERTS.md)。

- 狀態：**衝刺 8 規格（分支 `sprint8/ha-spec`，從 0.8.0 `ef78b44` 分出；已併入 `sprint8/integration`）**。只有文件，沒有程式碼變更。第 21 節的 20 項**已決定（2026-10-19，採建議）**，本文件的規則即依各項的建議撰寫。**0.9.0 的範圍**：L1a 儲存抽象與 L4 Pulse 多位址（第 20 節）；L1b PostgreSQL 後端（含 schema v9）與 L2 叢集協調（階段 A）延到 **0.10.0**。
- 商業審計 A2 的原文：「沒有 HA：Kairos 是單一行程，狀態放在記憶體，資料目錄假設只有一個寫入者。Pulse 只能設定一個伺服器位址。修正方向：讓 Kairos 只負責連線（無狀態），狀態與資料移到外部資料庫，前面放 L4 LB；Pulse 支援多個位址或 DNS SRV。6～10 人週。」
- 設計依據（設計包，唯讀）：`decisions/INTEGRATION.md` §26～§30 的代碼分配慣例、`design-system/copywriting.md` §3（代碼格式與號段）、`kairos/screens.md` S20（系統健康）。設計包需要配合的事項列在第 22 節，**本線沒有修改 `design/`**。
- 埠號寫法：本文件沿用文件慣例，以 8443 表示 agent 連線埠（`server.listen`）、8444 表示註冊通道（`enroll.listen`）、9443 表示主控台（`console.listen`）。注意 Kairos 程式內 `server.listen` 的預設值是 `127.0.0.1:8080`（`config.rs`），Pulse `server.port` 的預設值也是 8080。

## 0. 摘要

| 項目 | 建議 |
|---|---|
| 分階段 | **階段 A＝主動／備援**：兩個以上的 Kairos 共用一個 PostgreSQL，以資料庫租約選出唯一的「主動節點」，只有它監聽與服務；備援節點熱待命，主動節點失效時約 30～60 秒接手。**階段 B＝多主動連線層**：所有節點都接受 agent 連線（8443），單一寫入者的工作（告警、彙總、保留、CRL、稽核自我驗證等）仍只由 leader 執行。8444 與 9443 在階段 B 的第一步（B1）仍只由 leader 服務 |
| 資料庫 | **PostgreSQL 14 以上**（社群版功能即可）。SQLite 維持為單機的預設後端，不移除；SQLite＋Litestream／LiteFS 不作為產品支援的 HA 方案（第 4 節） |
| 協調 | 不引入 etcd、Consul 等新元件：資料庫中的租約列（`cluster_leases`）加 fencing epoch。PostgreSQL 的 `LISTEN/NOTIFY` 只用來縮短延遲，正確性一律靠輪詢資料庫的版本號 |
| Pulse | **不改也能用**：前面放 L4 負載平衡器或 VRRP 虛擬 IP，Pulse 照舊只設一個位址。另外新增 `server.fallback_addresses`（多個位址，依序改試）與選用的 DNS SRV；新增 feature `cluster`（`hello_ack.server_node` 與排空訊息 `drain`），**協定版本不變** |
| 機密 | 所有節點必須持有同一組金鑰（`alerts-secrets.key`、`console.key`、`audit-hmac.key`、簽發 CA 私鑰）。叢集模式下**不再自動產生金鑰**，啟動時比對資料庫中記錄的指紋，不符合時不進入就緒 |
| 稽核鏈 | 在 PostgreSQL 上以 `meta.audit_chain_head` 的列鎖（`SELECT … FOR UPDATE`）序列化所有寫入者（各節點與 CLI），`id` 在持鎖期間指定，鏈的順序等於 commit 的順序 |
| 升級 | 0.8（單機 SQLite）→ 0.9（單機，schema v9）→ 停機以 `kairos db migrate` 搬到 PostgreSQL → 啟動第一個節點 → 複製金鑰與設定、啟動第二個節點 → 設定 LB 或 VIP |
| 估工 | 階段 A 約 11～13 人週，階段 B 另加 4～6 人週（含 Pulse 多位址與 SRV）。比審計估的 6～10 人週多，原因見第 20.3 節 |

## 1. 目標與非目標

### 1.1 目標

1. **單一節點故障不中斷服務**：任何一個 Kairos 節點（行程當掉、主機斷電、網路隔離）失效時，agent 在 60 秒內（預設參數）重新連上其他節點，已 `ack` 的資料不遺失，未 `ack` 的資料由 Pulse 補送且不重複。
2. **沒有腦裂**：任何時刻最多一個節點執行單一寫入者的工作；舊 leader 恢復連線後不會覆寫新 leader 的結果（fencing）。
3. **維運可預期**：滾動式更換節點（排空、升級、重新加入）不需要停止整個服務（schema 不變的版本）；狀態、角色與異常都看得到（`/readyz`、`/metrics`、系統健康、`kairos cluster status`）。
4. **單機部署不受影響**：沒有設定 `[cluster]` 時，行為、效能與 0.8 相同（SQLite、單一行程）。
5. **協定相容**：協定版本維持 v1～v2；任何版本的 Pulse 都能連上叢集（只是舊版 Pulse 沒有多位址與排空的最佳化）。

### 1.2 非目標（本規格不處理）

- **資料庫本身的 HA**：PostgreSQL 的複寫、自動切換（Patroni、雲端代管服務、Pacemaker）由客戶或 DBA 負責，Kairos 只規定它需要的保證（第 4.3 節）與故障時的行為。
- **多區域、跨資料中心的多主動**、依地區分流與資料駐留。
- **時序資料庫**（TimescaleDB、ClickHouse 等）：PostgreSQL 後端先以一般資料表實作；分區與時序擴充是之後的效能工作（第 7.6 節）。
- **JSONL 後端的叢集**：`storage.backend = "jsonl"` 只支援單機。
- **把 SQLite 檔放在共用儲存上讓多個節點同時使用**：WAL 需要共享記憶體與 POSIX 鎖，Kairos 已拒絕在網路磁碟上啟動（STORAGE.md 第 2 節），本規格不改變。
- **授權（license）閘**：HA 是商業功能，但授權機制尚未實作（審計 B 系列），本規格不設計授權檢查，只在第 21 節列出（#14，已決定：等授權機制完成後再加）。
- **Kairos 完全無狀態**：審計的修正方向是「讓 Kairos 只負責連線（無狀態）」。本規格把**持久狀態**全部移到資料庫，但每條 agent 連線的工作階段（協商結果、在途的 report、速率限制的桶）仍在服務它的節點的記憶體中；節點失效時這些狀態由 Pulse 重連與補送重建，不需要跨節點複製。

## 2. 現況盤點：單機假設

0.8.0 的 Kairos 假設「一個行程寫一個本機 SQLite 檔」。互斥靠三樣東西：SQLite 的全域寫入鎖（`BEGIN IMMEDIATE`、31 處 `TransactionBehavior::Immediate`）、行程內的 `Mutex`，以及各服務記憶體中的狀態。跨行程的協調全部靠輪詢資料庫（`alerts_config_rev`、`settings`、`alert_actions`、`enroll_requests.state`）或 TLS 檔案的內容指紋。程式中沒有 leader 選舉、租約或 advisory lock。

**實際上能擋住第二個實例的只有兩件事**：

- 監聽埠被佔用（`KRS-E2001`，結束代碼 11）；
- 資料庫放在網路磁碟時無法啟用 WAL。

同一台主機換埠、或兩個行程指向同一個本機資料目錄時，兩個 Kairos 可以同時寫同一個 `kairos.db`，會發生下表中「多節點問題」欄的錯誤。

下表的「處置」欄：**A** 表示階段 A 需要處理，**B** 表示階段 B 才需要處理，**—** 表示不需要處理。

### 2.1 記憶體中的狀態

| # | 項目 | 位置 | 存放 | 多節點問題 | 處置 |
|---|---|---|---|---|---|
| M1 | 狀態表（連線表、`last_seen`、`last_report`、`kick`） | `registry.rs` `Registry` | 純記憶體；每 30 秒同步到 `hosts`；啟動時以 `Registry::restore` 放回 24 小時內的主機 | 各節點只看得到自己的連線。同一個 `agent_id` 連到兩個節點時不會互踢（`superseded` 只在行程內有效）。節點 B restore 回來的離線主機，在清冊同步時會蓋掉節點 A 正連著的主機的 `online` | A：只有主動節點有狀態表，不需改。B：連線歸屬寫入 `host_presence`、跨節點踢除（第 6.5 節） |
| M2 | 啟動時把所有主機標為離線 | `store/sqlite.rs` `SqliteStore::open`（`UPDATE hosts SET online = 0`） | 資料庫 | 任何節點重新啟動都會把其他節點上的線上主機標為離線 | A：接手時改為只處理失聯節點的主機。B：同左（第 6.6 節） |
| M3 | 連線編號 `next_conn_id()` | `registry.rs`（`static AtomicU64`） | 純記憶體 | 各節點重複，不能當跨節點的身分 | B：跨節點身分改用 `conn_epoch`（資料庫序列） |
| M4 | report 速率限制 | `rate.rs` `ReportLimiter` | 純記憶體 | 換節點就換一個新桶 | —：一台 agent 同時只有一條連線，換節點最多多出一次突發額度（10 則），可以接受 |
| M5 | 每 IP 同時連線、新連線速率 | `per_ip.rs` `PerIpLimiter` | 純記憶體 | 上限變成「每節點」。經 L4 LB 時所有連線的來源都是 LB 的位址 | A：支援 PROXY protocol v2，取得真正的來源位址（第 3.4 節）。B：文件明訂上限是每節點的值 |
| M6 | 註冊通道 L1～L10、`renew_8443` 限流 | `enroll/limits.rs` | 純記憶體 | 額度變成 N 倍 | A：只有主動節點服務，不需改。B1：8444 只由 leader 服務，不需改 |
| M7 | 註冊通道 L7 封鎖 | `InvalidTracker.blocks` | `enroll_blocks` 表，但**只在啟動時讀回** | 節點 A 的封鎖，節點 B 要重新啟動才看得到 | A：接手時讀回即可。B2：每 10 秒重新讀取 |
| M8 | 主控台驗證失敗限速、不存在帳號的鎖定 | `api/auth.rs` `FailureLimiter`、`users/mod.rs` `PhantomLockout` | 純記憶體 | 猜密碼的額度是 N 倍 | B1：9443 只由 leader 服務，不需改。B2：改存資料庫 |
| M9 | MFA 第二步的挑戰 | `api/account.rs` `LoginState.mfa` | 純記憶體（5 分鐘） | 第一步在 A、第二步在 B 會失敗 | B1：不需改。B2：改存資料庫 `login_challenges` |
| M10 | 撤回申請的 nonce | `enroll/mod.rs` `nonces` | 純記憶體 | 在 A 取得、到 B 撤回會失敗 | B1：不需改。B2：改存資料庫 `enroll_nonces` |
| M11 | 事件匯流排 | `bus.rs` `EventBus`（broadcast 8192） | 純記憶體 | leader 的告警引擎收不到其他節點的事件（被取代、身分驗證失敗、除役、TLS 重載、主機重新開機） | B：事件寫入 `cluster_events`，leader 讀取（第 6.7 節） |
| M12 | SSE 事件編號與保留 | `api/events.rs` `EventHub`（`<epoch>-<序號>`，4096 則） | 純記憶體 | 換節點一定 `resync`；只看得到本節點連線的上下線 | A：接手後 `resync`（與重新啟動相同）。B：SSE 的主機事件改以 `cluster_events` 為來源 |
| M13 | 主機列表的索引、上線趨勢、連線紀錄 | `api/hosts.rs` `HostIndex`、`api/overview.rs` `MinuteSeries`、`api/conn_log.rs` `ConnLog` | 純記憶體 | 只反映本節點的連線；接手後趨勢與連線紀錄清空 | A：接手後清空（與重新啟動相同）。B：改讀 `host_presence`（第 6.6 節） |
| M14 | 告警引擎（`streaks`、抖動、彙總、暖機期、事件流水號 `seq_by_day`） | `alerts/engine.rs` `Engine` | 記憶體；事件寫入資料庫 | 每個節點各自判定、各自開事件、各自通知。流水號一定撞號，而 `ON CONFLICT(event_id) DO UPDATE` 會在無聲無息中覆寫其他節點的事件 | A：只有 leader 執行；寫入加 fencing（第 13 節） |
| M15 | 通知派送（佇列、每管道 30 秒速率、連續失敗計數） | `alerts/notify/mod.rs` `Dispatcher`、`Pump` | 記憶體；`alert_deliveries` | 所有節點都載入同一批待送通知並各自送出 | A：只有 leader 執行；換手時可能重送一則，見第 13.3 節 |
| M16 | `alerts_heartbeat`（停機判斷） | `meta` | 資料庫，單一鍵 | 多節點互相覆寫，「重新啟動、停機」的判斷錯亂 | A：改為叢集的 leader 心跳（第 13.2 節） |
| M17 | 彙總的記憶體桶 | `store/rollup.rs` `Rollup` | 記憶體；寫出後推進 `hosts.rollup_until` | 不變式「記憶體中的桶一定在 `rollup_until` 之後」被打破，節點異常結束時彙總少算；遲到判斷用的是過期的快取 | A：只有主動節點寫 report，不需改。B：PostgreSQL 後端改由 leader 從原始資料產生彙總（第 7.3 節） |
| M18 | 計數器的「上一筆」 | `store/derive.rs` `CounterState` | 記憶體 | 換節點後速率出現缺口或重複 | B：隨 M17 由 leader 依時間序計算 |
| M19 | 序列、主機、boot 的快取 | `store/sqlite.rs` `Caches` | 記憶體 | 上限（`max_hosts`、`MAX_SERIES`、每主機序列數）各節點各算一份 | B：改以資料庫的唯一鍵與 `ON CONFLICT` 為準，上限為近似值 |
| M20 | TLS 重載的觸發 | `reload.rs` `trigger()`（行程內 `Notify`） | 記憶體 | 節點 A 產生的 CRL 只立即觸發 A | —：各節點每 2 秒從資料庫寫出 PKI 檔並觸發本機重載（已是以資料庫為準的設計，第 11.3 節） |
| M21 | 註冊服務的其他狀態（`paused`、`decommissioned`、`first_used`） | `enroll/mod.rs` `EnrollService` | 記憶體，部分啟動時讀入 | `decommissioned` 啟動後不再讀取；自動暫停由各節點各自判斷並各寫一筆稽核 | A：只在主動節點執行。B：`decommissioned` 每 2 秒隨設定一併重讀；自動暫停只由 leader 判定 |
| M22 | 背景清除（保留、容量、告警、註冊、稽核、工作階段） | `SqliteStore::maintain`、`AlertService::purge`、`EnrollService::periodic`、`api/maint.rs` | — | 每個節點都會跑，重複刪除並互相搶鎖 | A：只有 leader 執行 |
| M23 | 稽核的自我驗證與鏈尾外送 | `audit_monitor.rs` | — | 每個節點各寫一筆 `audit.verify`、各送一行鏈尾 | A：只有 leader 執行 |
| M24 | `/metrics` 計數、`WriteStats` | `metrics.rs`、`stats.rs` | 純記憶體 | 每節點各自計數 | —：Prometheus 端依 `instance` 加總；文件說明 |
| M25 | `agents.json` | `monitor.rs` | 本機檔案 | 共用資料目錄時互相覆蓋 | —：叢集要求每個節點有自己的本機資料目錄，`agents.json` 是節點本機的快照 |

### 2.2 資料庫（SQLite）中依賴「單一寫入鎖」才正確的地方

| # | 項目 | 位置 | 為什麼需要處理 |
|---|---|---|---|
| D1 | 稽核雜湊鏈 | `audit.rs` `append_with_key` | 先 INSERT 取得寫入鎖，再以 `id < ?` 找前一筆計算雜湊。在 PostgreSQL 的 READ COMMITTED 下兩個交易會讀到同一個前一筆，鏈會分岔；序列配出的 `id` 也可能與 commit 的順序不同 |
| D2 | 註冊 token 的使用次數 | `enroll/handlers.rs`、`store::consume_token` | `UPDATE … SET used = used + 1` 沒有 `WHERE used < max_uses`，靠交易內先檢查再更新 |
| D3 | 續簽的重放快取 | `replay_seen` 後 `replay_insert`（衝突時覆寫） | 並行時兩個請求都能通過檢查 |
| D4 | 簽發的冪等 | `pki_mgr.rs` `issue()` | 交易內檢查狀態；兩個節點可能各簽一張（序號不同，UNIQUE 擋不住） |
| D5 | CRL 序號 | `regenerate_crl()` 讀 `next_crl_number` | 需要序列化；多節點各自產生時主鍵衝突 |
| D6 | 告警設定版本 | `alerts/store.rs` `bump_config_rev`（先讀再加一） | 不是原子遞增 |
| D7 | 告警事件流水號 | `load_seq` 取 `max(event_id)` | 多寫入者時撞號 |
| D8 | 動作佇列 | `alert_actions`（沒有 claim） | 兩個節點會同時處理同一筆 |
| D9 | 主機、boot、序列的建立 | `INSERT OR IGNORE` 後 SELECT | PostgreSQL 上改 `ON CONFLICT DO NOTHING`，語意相同 |

### 2.3 檔案

| 檔案 | 現況 | 多節點下 |
|---|---|---|
| `data/kairos.db`、`-wal`、`-shm` | 單一寫入者 | 叢集改用 PostgreSQL；SQLite 只給單機 |
| `data/alerts-secrets.key`、`console.key`、`audit-hmac.key` | 找不到時**自動建立** | 每個節點會在無聲無息中產生不同的金鑰。叢集模式停用自動建立，改為分發同一份（第 11 節） |
| `data/pki/issuing-<ca_id>.key` | `kairos pki issuing-ca new` 在執行 CLI 的主機上建立 | 只有放了金鑰的節點能簽發（其他節點 `KRS-E1303`）。叢集要求每個節點都有（第 11.2 節） |
| `data/pki/issuing-cas.pem`、`crl/issuing.crl`、`issuing-cas-retired.pem` | 由資料庫重新寫出 | 每個節點各自從資料庫寫出，不需共用 |
| `agents.json`、日誌、`.retention-guard-logs.json` | 本機 | 節點本機；資料目錄不可共用 |
| TLS 伺服器憑證與私鑰 | 依 `server.reload_interval_secs` 熱重載 | 每個節點各自一份，SAN 規則見第 8.5 節 |

### 2.4 Pulse

| 項目 | 現況 | 多節點下 |
|---|---|---|
| 伺服器位址 | `server.address`＋`server.port`，只能一個；`TcpStream::connect` 交給 getaddrinfo，多筆 A／AAAA 依序嘗試，整段共用 5 秒的 `CONNECT_TIMEOUT` | 新增 `server.fallback_addresses` 與 `server.srv`（第 8 節） |
| 憑證驗證的名稱 | `server.domain`（SNI 與名稱驗證都用它，與 `address` 無關） | 所有位址共用同一個驗證名稱，因此所有節點的伺服器憑證都要含這個 SAN（第 8.5 節） |
| 重連 | 指數退避（2 秒加倍到 60 秒，±20%），連線穩定 30 秒才重置；連續被 `superseded` 時至少 30 秒 | 換位址不增加退避（第 8.3 節） |
| durable 補送 | 本機緩衝；斷線後從最舊的未確認記錄重送；去重由伺服器以 (agent_id, `boot_id`, `report_seq`) 判斷 | 共用資料庫後去重天然是全叢集的（第 7.2 節） |
| 憑證輪替 | 8443 `renew_request` 走目前的連線；過期續簽走 8444 | 任何節點都能簽發（第 11.2 節） |
| 狀態輸出 | `status_file.rs` `Connection.server` 是啟動時由設定組成的字串，不是實際連上的位址 | 改為實際連上的候選位址，另加 `server_node`（第 8.7 節） |

## 3. 部署拓撲與分階段

### 3.1 階段 0：作業層的冷備援（不改程式，0.8 即可）

給還不能升級的客戶的過渡作法，寫在 `packaging/INSTALL.md` 的附錄（實作線 L3）：

- 以 DRBD（同步模式）或儲存陣列的同步複寫把資料目錄複寫到備援主機，由 Pacemaker 控制「只有一台掛載並啟動 Kairos」；或
- 以 `kairos db backup` 加 `--include-keys` 定期備份，故障時在備援主機還原。

限制：切換時間是分鐘級；非同步複寫或定期備份會遺失最後一段已 `ack` 的資料（Pulse 收到 `ack` 就刪除本機資料，無法補回）。**不建議用 Litestream 做 HA**：它是非同步複寫到物件儲存，適合異地備份，不能保證已 `ack` 的資料不遺失，還原也需要時間。

### 3.2 階段 A：主動／備援（共用 PostgreSQL）

```text
              Pulse（server.address = kairos.example.com，或另設 fallback_addresses）
                 │
        ┌────────┴─────────┐  L4 LB（TCP 透傳，健康檢查 /readyz）或 VRRP 虛擬 IP（keepalived）
        ▼                  ▼
  ┌──────────┐       ┌──────────┐
  │ Kairos A │       │ Kairos B │      每個節點有自己的本機資料目錄（日誌、金鑰、PKI 快取）
  │ 主動     │       │ 備援     │      備援：連著資料庫、續約失敗就接手，不監聽 8443／8444／9443
  │ leader   │       │          │
  └────┬─────┘       └────┬─────┘
       │  租約、心跳、資料  │
       └────────┬─────────┘
                ▼
       PostgreSQL（主庫＋同步備庫，由客戶的 HA 機制切換）
```

- **只有持有 leader 租約的節點是主動節點**：它監聽 8443、8444、9443，執行全部的工作，與 0.8 的單一行程完全相同（記憶體中的狀態表、事件匯流排、告警引擎、SSE 都不需要跨節點）。
- **備援節點**連著資料庫、寫心跳、每 2 秒嘗試取得租約，**不監聽** agent、註冊與主控台的連接埠（只監聽 `observability.listen`，`/readyz` 回 503 `standby`）。理由：不監聽就不會有「兩個節點都接受 agent 連線」的情況，階段 A 不需要處理 M1、M5～M13。
- **接手**：主動節點失效 → 租約在 `cluster.lease_ttl_secs`（預設 15 秒）後到期 → 備援節點取得租約、epoch 加一 → 依 0.8 的啟動流程載入狀態（第 6.6 節，只把失聯節點的主機標為離線）→ 開始監聽 → LB 的健康檢查轉向它，或 keepalived 把虛擬 IP 移過來 → Pulse 重連。
- **失去租約的主動節點**（續約失敗超過 `lease_ttl_secs − 5` 秒，或發現租約已被別人取得）立即停止接受連線，並**以結束代碼 75 結束行程**（`KRS-W8402`）。由 systemd 的 `Restart=always` 重新啟動，重新以備援身分加入。不嘗試「降級但保留行程」：記憶體中的狀態表、告警引擎與快取全部重來，最簡單也最不容易出錯。
- **預期的切換時間**（預設參數）：

  | 階段 | 時間 |
  |---|---|
  | 租約到期 | ≤ 15 秒 |
  | 備援取得租約（每 2 秒嘗試一次） | ≤ 2 秒 |
  | 載入狀態、開始監聽 | 約 2～5 秒（1 萬台主機的清冊載入，STORAGE.md 第 11.2 節） |
  | LB 健康檢查轉向（每 2 秒一次，連續 2 次成功） | 約 4 秒 |
  | Pulse 偵測斷線 | 主機還在、TCP 被重設時立即偵測到；主機斷電時要等讀取閒置逾時，預設 30 秒，可與上面幾個階段重疊 |
  | Pulse 重連（2 秒退避加 0～5 秒的隨機分散） | ≤ 7 秒 |

  合計約 25～35 秒（行程當掉）到 45～60 秒（主機斷電）。期間的取樣由 Pulse 的本機緩衝保存，連上後補送。
- **沒有 LB 時**：建議 keepalived 的 VRRP 虛擬 IP，以 `track_script` 檢查本機的 `/readyz`，虛擬 IP 永遠跟著主動節點。工廠與政府機關的地端環境多半沒有 LB，這是成本最低的作法。Pulse 也可以不依賴虛擬 IP，直接設定 `fallback_addresses`（第 8 節）。

### 3.3 階段 B：多主動連線層

```text
        Pulse ──► L4 LB（或 DNS SRV／fallback_addresses）
                   │        │        │
                   ▼        ▼        ▼
              Kairos A  Kairos B  Kairos C   全部接受 8443；其中一個是 leader
               (leader)                       （B1：8444、9443 只由 leader 服務）
                   └────────┬────────┘
                            ▼
                       PostgreSQL
```

- **8443 多主動**：每個節點都接受 agent 連線、寫 report 與資產（各自的 writer task、各自的 group commit）。連線的歸屬寫入 `host_presence`，同一個 `agent_id` 的新連線在任何節點出現時，舊節點收到通知後以 `superseded` 關閉舊連線（第 6.5 節）。
- **單一寫入者的工作只由 leader 執行**（第 6.3 節）。leader 失效時只有這些工作暫停 `lease_ttl_secs`，**agent 連線不受影響**。
- **B1（建議的第一步）**：8444 與 9443 仍只由 leader 服務。LB 對這兩個埠以 `/readyz?scope=enroll`、`/readyz?scope=console` 判斷，非 leader 回 503。註冊與主控台的量很小，不需要水平擴充；這樣 M6～M10 都不需要處理。
- **B2（之後，視需求）**：9443、8444 也多主動，需要把 M7～M10 移到資料庫，並提供跨節點的 SSE（第 10.3 節）。

### 3.4 L4 負載平衡器的要求

| 項目 | 要求 |
|---|---|
| 模式 | **TCP 透傳**（HAProxy `mode tcp`、F5 Fast L4、雲端 NLB）。8443、8444 是 mTLS，**LB 不可終結 TLS**，否則 Kairos 拿不到用戶端憑證。9443 也建議 L4；若用 L7，代理必須保留原本的 `Host`（CSRF 檢查 `Origin == https://<Host>`，API.md 第 12.3 節），且 `/metrics` 必須設定 token（`KRS-W0304`） |
| 健康檢查 | HTTP `GET /readyz`，送往每個節點的 `observability.listen`（純 HTTP，例如 `:9464`）。8443 用 `/readyz?scope=agent`，8444 用 `?scope=enroll`，9443 用 `?scope=console`（第 14.1 節）。間隔 2 秒，連續 2 次成功才加入、2 次失敗就移除 |
| 來源位址 | 建議 LB 送出 **PROXY protocol v2**（HAProxy `send-proxy-v2`），Kairos 以 `server.proxy_protocol_from`（信任的 LB 位址清單，CIDR）解析並取得真正的來源位址。用途：每 IP 限制（M5）、註冊的 `observed_ip` 與 L2～L7、稽核的 `actor_ip`、主機的 `peer`。不能送 PROXY 時改用透明模式（DSR、保留來源位址的 NAT）；兩者都做不到時，必須把 LB 的位址加入 `limits` 的允許清單，並接受每 IP 限制失效 |
| 逾時 | 閒置逾時至少 120 秒（Pulse 心跳預設 15 秒）；不要讓 LB 主動回 RST 以外的半開連線 |
| 黏著 | 不需要（8443 每台 agent 只有一條長連線；B1 的 9443 只有一個後端） |

PROXY protocol 的規則（實作線 L2）：

- 來源位址落在 `server.proxy_protocol_from`（`enroll.`、`console.` 各有同名設定，預設沿用 `server.` 的值）時，**必須**帶合法的 v2 標頭，否則關閉連線。
- 不在清單內的來源**不得**帶標頭，帶了就關閉連線。
- 只接受 v2（二進位）、`PROXY` 命令、TCP over IPv4／IPv6；`LOCAL` 命令（LB 的健康檢查）視為 LB 自己的連線。
- 標頭在 TLS 握手之前讀取，讀取時限沿用 TLS 握手的時限。
- 不合法時記 `KRS-W2206`（暫定，節流）。

## 4. 資料庫方案比較與建議

### 4.1 比較

| 方案 | 寫入者 | 已 `ack` 的資料遺失（RPO） | 切換 | 叢集協調 | 實作量 | 維運 | 結論 |
|---|---|---|---|---|---|---|---|
| **PostgreSQL**（主庫＋同步備庫） | 多個（任何節點） | 0（同步複寫且 `synchronous_commit = on`） | 由 Patroni、代管服務等自動切換，約 10～30 秒 | 資料庫中的租約與列鎖，不需新元件 | 大：所有 store 層改寫並發語意（第 5 節） | 客戶多半已有 DBA 與備份工具（`pg_dump`、pgBackRest、PITR） | **建議** |
| SQLite＋LiteFS | 一個（持有 Consul 租約的主節點），其他節點唯讀 | 預設非同步，> 0 | 依 Consul 租約，秒級 | 需要 Consul | 中：寫入要轉送到主節點 | FUSE、只支援 Linux；Fly.io 已停止 LiteFS Cloud，專案的維護活躍度下降 | 不建議 |
| SQLite＋Litestream | 一個 | > 0（非同步到物件儲存） | 手動或腳本還原，分鐘級 | 無 | 小 | 簡單 | 只當備份，不當 HA（第 3.1 節） |
| SQLite＋DRBD／Pacemaker（冷備援） | 一個 | 0（DRBD 同步模式） | 分鐘級（重新掛載、啟動、載入） | Pacemaker | 0 | 需要熟悉 Pacemaker | 階段 0 的過渡作法 |
| rqlite、dqlite（Raft 化的 SQLite） | 一個（Raft leader） | 0 | 秒級 | 內建 | 大：rqlite 是 HTTP API，dqlite 需要 C 函式庫與 cgo 類綁定 | 客戶不熟悉 | 不建議 |
| CockroachDB、YugabyteDB（PostgreSQL 相容的分散式資料庫） | 多個 | 0 | 秒級 | 內建 | 同 PostgreSQL，另有 SERIALIZABLE 重試 | 授權與維運成本高 | 不在本規格範圍，之後可評估 |

### 4.2 建議

- **叢集只支援 PostgreSQL**；**單機的預設仍是 SQLite**。兩種後端共用同一套 schema 版本號（v9 起）與同一套功能，以 `storage.backend = "postgres"` 選擇。
- 最低版本 **PostgreSQL 14**（RHEL 9、Ubuntu 22.04 的系統套件即為 13～14 以上；14 起 `pg_stat_*` 與 `idle_session_timeout` 等維運功能較完整）。只用社群版的功能：`ON CONFLICT`、`RETURNING`、`FOR UPDATE`、`LISTEN/NOTIFY`、部分索引、`jsonb`。**不依賴任何擴充**（TimescaleDB、pg_partman 等）。
- **驅動**：`postgres`（同步 API，內部是 `tokio-postgres`）加 `tokio-postgres-rustls`（rustls＋ring，與既有的加密後端相同），授權都是 MIT 或 Apache-2.0，`deny.toml` 的允許清單不需變更。選同步 API 的原因：現有的 store 層全部是在 blocking 執行緒上呼叫的同步函式（`ReportStore` 等），改用同步驅動可以保留這個結構。不選 `sqlx`：依賴多、巨集需要編譯期資料庫或離線快取，而且一樣要逐一處理並行控制。連線池以自行實作的小型池（每節點預設 16 條，`storage.postgres.pool_size`）取代 r2d2 等新依賴（第 21 節 #4，已決定採 (a)）。

### 4.3 Kairos 對 PostgreSQL 的要求

| 項目 | 要求 | 不符合時 |
|---|---|---|
| 版本 | ≥ 14 | 拒絕啟動，`KRS-E4402` |
| 編碼 | `UTF8` | 拒絕啟動，`KRS-E4402` |
| 權限 | 對 `storage.postgres.schema`（預設 `kairos`）有 `CREATE`、`USAGE`；遷移時需要建立資料表與索引 | 拒絕啟動，`KRS-E4402` |
| 已 `ack` 的資料不遺失 | `synchronous_commit` 為 `on`（或 `remote_write`、`remote_apply`），且有同步備庫（`synchronous_standby_names` 非空）或代管服務保證同步複寫 | **照常啟動**，啟動時與每小時記一次 `KRS-W4403`；系統健康的 `cluster` 項目顯示警告。理由：沒有同步備庫時，主庫故障會遺失最後幾個已 `ack` 的交易，等同 0.8 的 `sqlite_synchronous = "normal"` 遇到斷電 |
| TLS | 非 loopback 或 Unix socket 時預設 `sslmode = "verify-full"`（`storage.postgres.ca_file`） | 拒絕啟動，`KRS-E4402`；明確設 `sslmode = "disable"` 時照常啟動並記 `WARN` |
| 連線數 | `max_connections` ≥ 節點數 ×（`pool_size` ＋ 4） | 啟動時估算，不足時記 `WARN` |
| 時鐘 | 租約使用資料庫的時間（`clock_timestamp()`），節點時鐘不影響互斥；節點與資料庫的時差超過 2 秒時記 `KRS-W8305`（資料的時間戳記仍是節點的時間） | 只警告 |
| 連線池中介（PgBouncer） | 只支援 **session pooling**：`LISTEN` 與 `FOR UPDATE` 需要固定的連線。transaction pooling 不支援 | 偵測不到；文件說明 |

## 5. 儲存抽象與 PostgreSQL 後端

### 5.1 現況

- **只有 report 路徑有抽象**：`trait ReportStore`（`write_batch`、`sync_inventory`、`maintain`、`close`、`write_assets`；同步 API、一批一個交易），由單一 writer task 驅動（STORAGE.md 第 1 節）。
- **其他模組直接用 rusqlite**：函式簽章是 `&rusqlite::Connection` 或 `Transaction`。SQL 呼叫點約 381 處，分布如下：

  | 模組 | 呼叫點 |
  |---|---|
  | `store/sqlite.rs` | 55 |
  | `alerts/store.rs` | 55 |
  | `enroll/store.rs` | 48 |
  | `users/store.rs` | 45 |
  | `audit.rs` | 45 |
  | `pki/store.rs` | 30 |
  | `store/import.rs` | 22 |
  | `api/metrics.rs` | 13 |
  | 其他（api、CLI、probe、diag） | 約 70 |

- **連線的種類**：report writer 一條、主控台唯讀池 4 條、主控台低頻寫入 2 條（`api/db.rs`）、`EnrollDb`（一條寫入加 4 條唯讀，告警與稽核監看共用這個型別），以及各個 CLI 以 `open_admin` 開啟的連線。

### 5.2 目標分層

```text
         呼叫端（連線、API、告警、註冊、CLI）
                   │ 只認得領域介面
                   ▼
 ┌─────────────────────────────────────────────────────────┐
 │ 領域 store（trait）                                      │
 │ ReportStore（既有）、HostStore、AlertStore、EnrollStore、 │
 │ PkiStore、UserStore、AuditLog、SettingsStore、ApiQueries、│
 │ ClusterStore（新）                                       │
 └─────────────┬───────────────────────────┬───────────────┘
               ▼                           ▼
     sqlite::*（既有的 SQL 搬入）     postgres::*（新）
               │                           │
       rusqlite::Connection         postgres::Client（連線池）
```

- **領域介面以「一個交易能完成的業務操作」為單位**，例如 `EnrollStore::submit_request`（檢查 token、建立申請、扣次數、寫稽核）。不以「一條 SQL」為單位。理由：
  - 每個操作在兩種後端的並行控制寫法不同：SQLite 靠 `BEGIN IMMEDIATE`，PostgreSQL 靠條件式 UPDATE 或列鎖；
  - 呼叫端不應該知道後端是哪一種。
- **交易介面**：`trait Tx`，提供 `execute`、`query_row`、`query_map` 等；兩個後端各自實作。SQL 文字放在各後端自己的模組裡，**不做 SQL 方言轉換器**。只有完全相同的 SQL（多數 SELECT）可以共用常數：共用的 SQL 以 `?1`、`?2` 撰寫（SQLite 的編號佔位符），PostgreSQL 後端在準備敘述時機械式地換成 `$1`、`$2`。不使用 SQLite 的 `$名稱` 形式：SQLite 把 `$1` 當成具名參數、依第一次出現的順序編號，與 PostgreSQL 的語意不同。
- **CLI 共用同一套領域介面**：所有寫資料庫的 CLI（`kairos enroll`、`alerts`、`users`、`api-token`、`audit`、`pki`）改用領域 store，依設定檔的 `storage.backend` 連線，不再直接開 rusqlite。
- **遷移的順序**：
  1. 先把既有的 SQL 原封不動搬進 `sqlite::*`，跑完整的測試確認行為不變（純重構）。
  2. 再寫 `postgres::*`。
- **測試**：領域介面的測試以參數化執行兩個後端（`#[cfg(feature = "pg-tests")]` 加環境變數 `KAIROS_TEST_PG_URL`），在 kp-build 上以容器啟動 PostgreSQL 執行。沒有設定時只跑 SQLite。

### 5.3 SQL 方言的差異（實作清單）

| SQLite | PostgreSQL | 處數 |
|---|---|---|
| `WITHOUT ROWID` | 刪除 | 20（schema） |
| `INTEGER PRIMARY KEY`（rowid 別名） | `BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY`；需要保留遷移前的值時，搬移後以 `setval` 對齊 | 多處 |
| `AUTOINCREMENT` | 同上 | 2（`boots`、`users`） |
| `BLOB` | `BYTEA` | `reports.body` 等 |
| 無型別欄位（`meta.value`、`reports.agent_ts`） | `meta.value` 改 `TEXT`（數值以文字保存）；`agent_ts` 拆成 `agent_ts_ms BIGINT` 與 `agent_ts_text TEXT` | 2 |
| `INSERT OR IGNORE` | `ON CONFLICT DO NOTHING` | 6 |
| `INSERT OR REPLACE` | `ON CONFLICT (…) DO UPDATE` | 1 |
| `last_insert_rowid()` | `RETURNING id` | 5 |
| 兩參數的 `max(a, b)`、`min(a, b)` | `GREATEST`、`LEAST` | 4 |
| `json_object`、`json_extract` | `jsonb_build_object`、`->>` | 2（`api/maint.rs`） |
| `COLLATE NOCASE`（`users.username`） | 唯一索引 `lower(username)`，比對一律 `lower()` | 1 |
| `LIKE … ESCAPE '\'` | 相同（PostgreSQL 的 `LIKE` 區分大小寫，原本依賴不分大小寫的地方改 `ILIKE`） | 1 |
| `sqlite_master` | `information_schema` | 2 |
| `PRAGMA user_version`、`application_id` | `kairos.meta` 的 `schema_version`、`application`（值 `KRS1`） | — |
| `PRAGMA journal_mode`、`synchronous`、`wal_checkpoint`、`page_count` | 不適用；容量改以 `pg_total_relation_size` 計算（第 7.5 節） | 約 30 |
| `VACUUM INTO`（`kairos db backup`） | 不適用（第 16.3 節） | 1 |
| `ON CONFLICT … DO UPDATE SET … excluded.` | 語法相同 | 24 |
| `RETURNING` | 相同 | 3 |
| 時間函式 | 無（時間一律是 Unix 毫秒的整數） | 0 |

### 5.4 並行控制的改寫（PostgreSQL 一律 READ COMMITTED）

| # | 操作 | PostgreSQL 的寫法 |
|---|---|---|
| D1 | 稽核鏈 | 第 12 節 |
| D2 | token 扣次數 | `UPDATE enroll_tokens SET used = used + 1 WHERE token_id = $1 AND used < max_uses AND …可用條件 RETURNING used`；影響 0 列時視同 token 已用完。申請的 INSERT 在同一個交易，扣不到時回復 |
| D3 | 重放快取 | `INSERT INTO enroll_replay … ON CONFLICT (key) DO NOTHING RETURNING key`；沒有回傳列＝重放。不再先 SELECT |
| D4 | 簽發冪等 | 先搶占：`UPDATE enroll_requests SET state = 'issuing', issuing_node = $node, issuing_at = $now WHERE id = $1 AND state = 'approved' RETURNING …`；搶到的節點才簽章，在同一個交易寫入憑證並改為 `issued`。交易失敗時整個回復，狀態回到 `approved`，不會卡在 `issuing`。另以 `(agent_id, csr_sha256)` 的唯一索引作為最後防線。8444 的同步簽發（自動核准、過期續簽）與 8443 的 `renew_request` 同樣先搶占 |
| D5 | CRL 序號 | 只有 leader 產生 CRL；交易內 `SELECT next_crl_number FROM pki_cas WHERE ca_id = $1 FOR UPDATE`，加上 leader 的 fencing（第 6.2 節） |
| D6 | 設定版本 | 新表 `cluster_revs(name, rev)`：`UPDATE cluster_revs SET rev = rev + 1 WHERE name = $1 RETURNING rev`。`alerts_config_rev` 搬到這裡（名稱 `alerts_config`）；另有 `settings`、`pki`、`enroll_blocks` 等版本號，第 6.4 節 |
| D7 | 告警事件流水號 | 新表 `alert_event_seq(day TEXT PRIMARY KEY, seq BIGINT)`：`INSERT … ON CONFLICT (day) DO UPDATE SET seq = alert_event_seq.seq + 1 RETURNING seq`。告警引擎只在 leader 執行，這張表是防止換手時撞號的保險 |
| D8 | 動作佇列 | 只有 leader 處理；以 `SELECT … FOR UPDATE SKIP LOCKED LIMIT 100` 取出、處理、標記 `processed_at`，都在同一個交易 |
| D9 | 主機、boot、序列 | `INSERT … ON CONFLICT DO NOTHING RETURNING id`，沒有回傳時再 SELECT。快取只會新增、不會改變，跨節點安全 |
| — | 工作階段的最後活動、token 的最後使用 | 已是條件式 UPDATE（`WHERE last_used_at < $new`），不需改 |
| — | 登入失敗計數、TOTP 防重放 | 已是條件式或原子 UPDATE（`failed_logins = failed_logins + 1 … RETURNING`、`totp_last_step < $1`），不需改 |

**鎖的順序**：所有交易依「主機或業務列 → `pki_cas` → `cluster_leases`（fencing，FOR SHARE）→ `meta.audit_chain_head`（FOR UPDATE，一律最後）」的順序取得列鎖，避免死結。PostgreSQL 偵測到死結（`40P01`）或序列化失敗（`40001`）時，領域 store 自動重試整個交易最多 3 次，仍失敗時回傳暫時性錯誤（report 回 `storage_unavailable`，API 回 503）。

### 5.5 schema v9

v9 同時套用到 SQLite 與 PostgreSQL，兩者的資料表與欄位一致（型別依第 5.3 節對應）。單機的 SQLite 也建立這些資料表，只是叢集用的表保持空白；這樣「SQLite → PostgreSQL」的搬移是一對一的複製。

```sql
-- 叢集的節點（每個節點每 cluster.heartbeat_secs 秒更新一次）
CREATE TABLE cluster_nodes (
    node_id        TEXT PRIMARY KEY,      -- cluster.node_id（預設為主機名稱，小寫）
    hostname       TEXT NOT NULL,
    version        TEXT NOT NULL,         -- Kairos 版本與 commit
    schema_version INTEGER NOT NULL,
    role           TEXT NOT NULL,         -- starting / standby / leader / member / draining / stopped
    config_hash    TEXT NOT NULL,         -- 需要一致的設定的雜湊（第 15.2 節）
    keys_hash      TEXT NOT NULL,         -- 金鑰指紋的雜湊（第 11.1 節）
    advertise      TEXT,                  -- 顯示用的位址
    started_at     BIGINT NOT NULL,
    heartbeat_at   BIGINT NOT NULL,       -- 資料庫時間，Unix 毫秒
    info           TEXT                   -- JSON：連線數、監聽位址、最近的錯誤（系統健康使用）
);

-- 租約（v9 只有 'leader' 一列；之後可以把工作拆成多個租約）
CREATE TABLE cluster_leases (
    name        TEXT PRIMARY KEY,
    holder      TEXT NOT NULL,            -- node_id
    epoch       BIGINT NOT NULL,          -- 每次換手加一，fencing token
    acquired_at BIGINT NOT NULL,
    expires_at  BIGINT NOT NULL           -- 資料庫時間
);

-- 跨節點通知用的版本號（設定、PKI、告警設定、封鎖清單……）
CREATE TABLE cluster_revs (name TEXT PRIMARY KEY, rev BIGINT NOT NULL);

-- agent 連線的歸屬與即時狀態（階段 B 使用；階段 A 由主動節點寫入，供接手時判斷）
CREATE TABLE host_presence (
    host_id        BIGINT PRIMARY KEY,    -- hosts.host_id
    conn_node      TEXT,                  -- 目前服務它的節點；離線時保留最後一個
    conn_epoch     BIGINT NOT NULL,       -- 每次 hello 由序列 conn_epoch_seq 配發，跨節點遞增
    online         INTEGER NOT NULL,
    connected_at   BIGINT,
    last_seen      BIGINT NOT NULL,       -- 節點時間；線上時每 15 秒更新
    offline_reason TEXT,
    peer           TEXT,
    rev            BIGINT NOT NULL        -- 由序列 presence_rev_seq 配發，變更追蹤用
);
CREATE INDEX host_presence_rev ON host_presence (rev);
CREATE INDEX host_presence_node ON host_presence (conn_node) WHERE online = 1;

-- 跨節點事件（階段 B：SSE 的主機事件、送往 leader 告警引擎的匯流排事件、踢除通知）
CREATE TABLE cluster_events (
    id      BIGINT PRIMARY KEY,           -- 序列；PostgreSQL 為 IDENTITY
    at      BIGINT NOT NULL,
    node_id TEXT NOT NULL,                -- 發布者
    kind    TEXT NOT NULL,                -- host.online / host.offline / bus.<type> / kick / drain
    target  TEXT,                         -- kick：被踢的節點
    payload TEXT NOT NULL                 -- JSON
);

CREATE TABLE alert_event_seq (day TEXT PRIMARY KEY, seq BIGINT NOT NULL);

-- B2 才使用（主控台與註冊通道多主動時）
CREATE TABLE login_challenges (
    challenge_hash BYTEA PRIMARY KEY,
    user_id BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    attempts INTEGER NOT NULL
);
CREATE TABLE enroll_nonces (
    nonce_hash BYTEA PRIMARY KEY,
    request_id TEXT NOT NULL,
    expires_at BIGINT NOT NULL
);
CREATE TABLE auth_failures (
    source TEXT PRIMARY KEY,              -- 來源（IPv4、IPv6 /64）或不存在帳號的雜湊
    tokens REAL NOT NULL,
    updated_at BIGINT NOT NULL
);

-- 既有資料表的變更
ALTER TABLE enroll_requests ADD COLUMN issuing_node TEXT;   -- D4 的搶占
ALTER TABLE enroll_requests ADD COLUMN issuing_at BIGINT;
CREATE UNIQUE INDEX enroll_requests_issued_csr
    ON enroll_requests (agent_id, csr_sha256) WHERE state = 'issued';
ALTER TABLE alert_deliveries ADD COLUMN claim_epoch BIGINT;  -- 送出時的 leader epoch（第 13.3 節）
```

`meta` 的新鍵：

| 鍵 | 內容 |
|---|---|
| `cluster_id` | `kairos cluster init` 產生的 UUID |
| `cluster_keys` | 各金鑰的指紋（第 11.1 節） |
| `schema_upgraded_by` | 既有，最後一次升級 schema 的 Kairos 版本 |

- **遷移**：v9 是一個交易；PostgreSQL 由第一個以新版啟動的節點執行，執行前以 `pg_advisory_lock(<KRS1 的整數值>)` 取得遷移鎖，其他節點等待後重新檢查版本。
- **版本比程式新時**（叢集中已有較新的節點升級了 schema）：拒絕啟動，`KRS-E4405`。與 0.8 單機的「降版執行」規則相同，只是改給代碼。
- **0.8 的單機升級到 0.9 時**照常自動套用 v9（`hosts`、`alert_*` 等既有資料不變）；升級後 0.8 無法開啟（降版從備份還原），與歷次 schema 升級相同。

### 5.6 `kairos db migrate`：SQLite 與 PostgreSQL 互搬

```bash
# 停止 Kairos 後執行
sudo -u kairos kairos db migrate --to postgres --config /opt/kairos/kairos.toml [--reports-since 2d] [--dry-run]
# 反向（回到單機，或把 PostgreSQL 的內容匯出成一個可攜的 SQLite 檔）
sudo -u kairos kairos db migrate --to sqlite:/var/backups/kairos/export.db --config /opt/kairos/kairos.toml
```

- **來源與目的**：來源是設定中目前的後端，目的由 `--to` 指定；PostgreSQL 的連線參數取自設定的 `[storage.postgres]`。目的必須是空的（沒有 Kairos 的資料表）；已有資料時拒絕（`KRS-E4404`），不做合併。
- **一致性**：
  - 來源是 SQLite 時以獨占鎖開啟（與 `kairos reports import` 相同，Kairos 執行中時以 `KRS-E4304` 拒絕）。
  - 來源是 PostgreSQL 時以 `REPEATABLE READ` 快照讀取，且要求所有節點都已停止（`cluster_nodes` 中沒有 30 秒內的心跳），否則拒絕（`KRS-E4304`）。
- **方式**：依資料表的相依順序複製；PostgreSQL 端用 `COPY … FROM STDIN (FORMAT binary)`。保留所有主鍵值（`host_id`、`series_id`、`audit_events.id` 等），複製後以 `setval` 對齊各序列。每張表一個交易；中斷後重新執行會先清空目的端（需要 `--restart`）。
- **`--reports-since`**：只複製取樣時間在範圍內的原始 report 與行程快照，用來縮短停機時間；彙總、清冊、告警、稽核等一律全部複製。範圍外的原始資料不複製，在來源保留，可另外以 `kairos reports export` 取出。
- **驗證**（不可略過）：
  1. 每張表的列數；
  2. `reports` 的 `max(id)` 與抽樣比對 1,000 列的內容；
  3. 在目的端執行完整的稽核鏈驗證（雜湊鏈與 HMAC，需要 `audit-hmac.key`）。

  任何一項不一致都以 `KRS-E4404` 結束（結束代碼 1），目的端保留供檢查，不會被當成可用的資料庫：遷移完成前 `meta.schema_version` 不寫入，Kairos 會拒絕使用。
- **時間估算**（實作線 L3 需在 kp-build 量測後補上）：以每秒 10 萬列估算，1,000 台主機、原始資料 7 天（約 1.2 億列）約 20 分鐘；`--reports-since 1d` 約 3 分鐘。
- **輸出**：依 `cli-style.md`，每張表一行進度，最後是摘要；`--json` 的 `schema` 為 `kairos.db.migrate/v1`。

## 6. 叢集協調

### 6.1 節點與心跳

- 每個節點啟動時以 `cluster.node_id` 寫入 `cluster_nodes`（UPSERT），之後每 `cluster.heartbeat_secs`（預設 5 秒）更新 `heartbeat_at`（資料庫時間）、`role` 與 `info`。
- `node_id` 在叢集內必須唯一。啟動時發現同名節點的心跳在 `3 × heartbeat_secs` 內、且 `started_at` 不同時拒絕啟動（`KRS-E8403`，視為設定錯誤：兩台機器用了同一個 `node_id`）。
- leader 把心跳超過 `cluster.node_lost_secs`（預設 20 秒）的節點標為失聯（`role` 改 `stopped`、記 `KRS-W8406`），並處理它的連線（第 6.6 節）。
- 正常關機時，節點先把 `role` 改為 `stopped`、交還租約（若持有），再結束。

### 6.2 leader 租約與 fencing

**取得與續約**：單一敘述，時間一律用資料庫的 `clock_timestamp()`，節點的時鐘不影響互斥。

```sql
-- $1 = node_id，$2 = ttl_ms；now_ms = (extract(epoch from clock_timestamp()) * 1000)::bigint
INSERT INTO cluster_leases (name, holder, epoch, acquired_at, expires_at)
VALUES ('leader', $1, 1, now_ms, now_ms + $2)
ON CONFLICT (name) DO UPDATE SET
    epoch       = CASE WHEN cluster_leases.holder = excluded.holder
                       THEN cluster_leases.epoch ELSE cluster_leases.epoch + 1 END,
    acquired_at = CASE WHEN cluster_leases.holder = excluded.holder
                       THEN cluster_leases.acquired_at ELSE excluded.acquired_at END,
    holder      = excluded.holder,
    expires_at  = excluded.expires_at
WHERE cluster_leases.holder = excluded.holder          -- 續約
   OR cluster_leases.expires_at < excluded.acquired_at -- 已到期，接手
RETURNING holder, epoch, expires_at;
```

- **參數**：`cluster.lease_ttl_secs` 預設 15（5～120），續約間隔為 TTL 的 1/3（預設 5 秒）；非 leader 每 2 秒嘗試一次。
- **自我降級**：leader 以本機的單調時鐘記錄「最後一次續約成功的送出時間」。距今超過 `TTL − 5 秒`（預設 10 秒）仍沒有成功續約時，視為已失去租約：
  - 階段 A：結束行程（第 3.2 節）；
  - 階段 B：停止單一寫入者的工作，回到 member。
  
  記 `KRS-W8402`。資料庫切換（PostgreSQL 主庫故障）通常需要 10～30 秒，比預設的 TTL 長時，請把 `lease_ttl_secs` 調到大於資料庫的切換時間，否則每次資料庫切換都會造成一次 leader 換手。
- **fencing**：每個 leader 專屬工作的寫入交易，第一個敘述都是：

  ```sql
  SELECT 1 FROM cluster_leases
   WHERE name = 'leader' AND holder = $node AND epoch = $epoch AND expires_at > now_ms
   FOR SHARE;
  ```

  沒有回傳列時回復交易並自我降級。`FOR SHARE` 會讓接手的 UPDATE 等到這個交易結束，所以「舊 leader 的交易」與「新 leader 取得租約」不會重疊：舊 leader 的交易要嘛在接手前 commit，要嘛在接手後因為 epoch 不符而失敗。單機（沒有叢集）時 fencing 是空操作。
- **為什麼不只用 `pg_advisory_lock`**：
  - advisory lock 綁在連線上：連線斷掉就釋放，但持有者未必立即知道（網路隔離時 TCP 可能數分鐘後才斷）；
  - 沒有 epoch 可以拿來做 fencing；
  - 在 PgBouncer 的 transaction pooling 下無法使用；
  - 維運者在資料表中看不到誰是 leader。

  advisory lock 只用在 schema 遷移與 `kairos db migrate` 這種一次性的互斥。
- **主動交還**：`kairos cluster step-down`（或正常關機）把 `expires_at` 改為現在，其他節點在 2 秒內接手。

### 6.3 leader 專屬的工作

| 工作 | 現在的位置 | 階段 A | 階段 B |
|---|---|---|---|
| 告警引擎、通知派送、`alert_actions`、告警清除、`alerts_heartbeat` | `alerts/` | leader（＝主動節點） | leader；主機狀態改讀 `host_presence`（第 13.4 節） |
| 彙總寫出 | `SqliteStore::maintain` | leader（＝唯一的 writer，不變） | leader 從原始資料產生（第 7.3 節） |
| 原始資料與彙總的保留期限、容量上限、`boots` 清除 | 同上 | leader | leader |
| 清冊同步（`sync_inventory`） | writer 每 30 秒 | 主動節點 | 每個節點同步自己連線的主機（第 6.6 節） |
| CRL 產生、待生效撤銷的收錄 | `EnrollService::sync_pki` | leader | leader；各節點仍各自寫出本機 PKI 檔 |
| CLI 核准的申請的簽發（`issue_pending`） | `EnrollService` | leader | leader（以第 5.4 節 D4 的搶占，其他節點即使執行也安全） |
| 註冊的自動暫停判定、`expire_and_purge`、封鎖與放寬的清除、重放快取的清除 | `EnrollService::periodic` | leader | leader |
| 稽核的保留清除、自我驗證、鏈尾外送 | `audit_monitor.rs`、`purge_audit` | leader | leader |
| 工作階段的清除、token 稽核旗標 | `api/maint.rs` | leader | leader |
| 失聯節點的處理、`cluster_events` 清除（保留 1 小時或 10 萬則） | 新 | leader | leader |
| JSONL 鏡像 | writer | 不支援叢集（設定 `jsonl_mirror = true` 時叢集拒絕啟動，`KRS-E0202`） | 同左 |

實作上把這些工作集中到一個 `LeaderJobs` 監督器：取得租約時啟動，失去租約時取消。每個工作的寫入交易都經過 fencing。

### 6.4 跨節點的變更通知

既有的「每 2 秒或 5 秒輪詢資料庫」設計已經能跨行程運作（`settings`、`alerts_config_rev`、`enroll_requests` 的長輪詢），因此跨節點通知沿用這個模式：

- **正確性靠輪詢 `cluster_revs`**：寫入端在同一個交易遞增對應的版本號，讀取端每 2 秒讀一次 `cluster_revs`（一個小查詢），版本號改變時重新載入。

  | `name` | 何時遞增 | 讀取端 |
  |---|---|---|
  | `settings` | `settings` 表改變 | 所有節點的 `EnrollService::refresh_settings` |
  | `alerts_config` | 規則、靜音、管道改變（取代 `meta.alerts_config_rev`） | leader 的告警服務 |
  | `pki` | `pki_cas`、`pki_crls`、撤銷改變 | 所有節點的 `sync_pki`（寫出本機 PKI 檔、觸發 TLS 重載） |
  | `enroll_blocks` | L7 封鎖改變 | B2 的所有節點 |
  | `lifecycle` | 主機除役、恢復 | 所有節點的 `EnrollService.decommissioned` |
- **延遲靠 `LISTEN/NOTIFY` 縮短**：寫入端 commit 後送 `NOTIFY kairos_rev, '<name>'`（另有 `kairos_kick`、`kairos_events`）；每個節點以一條專用連線 `LISTEN`，收到時立即檢查，不等下一次輪詢。NOTIFY 遺失（連線重建、資料庫切換）只會讓延遲回到 2 秒，不影響正確性。
- SQLite 單機不需要 NOTIFY：行程內的 `wake()` 與 `reload::trigger()` 照舊。

### 6.5 agent 連線的歸屬與跨節點的 `superseded`（階段 B）

收到合法的 `hello` 時（已通過 mTLS 的身分檢查，冒用者無法踢掉別人），節點在一個短交易中登記歸屬：

```sql
WITH prev AS (SELECT conn_node, conn_epoch, online FROM host_presence WHERE host_id = $1 FOR UPDATE)
INSERT INTO host_presence (host_id, conn_node, conn_epoch, online, connected_at, last_seen, peer, rev)
VALUES ($1, $node, nextval('conn_epoch_seq'), 1, $now, $now, $peer, nextval('presence_rev_seq'))
ON CONFLICT (host_id) DO UPDATE SET conn_node = excluded.conn_node, conn_epoch = excluded.conn_epoch,
    online = 1, connected_at = excluded.connected_at, last_seen = excluded.last_seen,
    offline_reason = NULL, peer = excluded.peer, rev = excluded.rev
RETURNING conn_epoch, (SELECT conn_node FROM prev), (SELECT conn_epoch FROM prev), (SELECT online FROM prev);
```

- 先前的歸屬是**別的節點且仍在線上**時，在同一個交易寫入 `cluster_events`（`kind = 'kick'`、`target = <舊節點>`、`payload = {agent_id, conn_epoch: <舊 epoch>}`），commit 後 `NOTIFY kairos_kick`。
- 舊節點收到通知（或每 2 秒輪詢 `cluster_events` 中 `target = 自己`）時，若本機的這條連線的 epoch 等於通知中的舊 epoch，就以 `superseded`（fatal）關閉它，並發布 `host_superseded`（帶 `previous_ip`、`ip`）。行為與 0.8 的行程內踢除相同，Pulse 的「連續被 `superseded`」規則照舊生效（兩台主機共用同一份身分時不會快速互踢）。
- **舊連線在被踢之前寫入的資料**：
  - durable 的 report 由唯一索引去重；
  - 非 durable 的 report 是當時的真實取樣，照常保存；
  - 狀態更新（`last_seen`、離線）一律帶 `WHERE conn_epoch = $我的 epoch`，舊連線不會蓋掉新連線的狀態。
- 同一個 `agent_id` 的兩條連線在兩個節點上同時 hello：`FOR UPDATE` 讓兩者依序執行，後者踢掉前者，結果與單機相同。
- 階段 A 只有主動節點寫 `host_presence`（每次 hello、離線時），不需要踢除；寫入它是為了讓接手的節點知道哪些主機是由失聯的節點服務的。

### 6.6 主機狀態的全域檢視

**線上狀態的來源**：

- **階段 A**：主動節點記憶體中的狀態表（與 0.8 相同）。
- **階段 B**：`host_presence`。每個節點：
  - 上線、離線時立即寫入（每 100 ms 合併成一個交易）；
  - 線上主機的 `last_seen` 每 15 秒更新一次（1 萬台約每秒 670 列，PostgreSQL 的 HOT update 足以應付；獨立成一張表，不讓 `hosts` 的大列反覆改寫）；
  - `hosts` 的清冊欄位（hello 的主機資訊、`last_report`）仍依 0.8 的規則每 30 秒同步，但只同步自己連線的主機，且帶 `conn_epoch` 條件。

**取代「啟動時把所有主機標為離線」（M2）**：

- **單機**：照舊。
- **叢集**：節點啟動時只把 `conn_node = 自己` 的線上主機標為離線（原因 `server_shutdown` 或 `server_lost`，看上次是否正常關機）。leader 發現節點失聯時，把 `conn_node = 失聯節點` 的線上主機標為離線，原因 **`server_lost`**（新的離線原因，第 21 節 #11）。
- 這些主機的 Pulse 會在數十秒內連上其他節點並重新上線。告警的主機離線寬限預設 3 分鐘，因此正常不會產生告警。

**階段 B 的 `ClusterView`**：

- 每個節點維護一份全叢集主機狀態的快取：啟動時全量讀取 `host_presence`，之後每 2 秒讀取 `rev > 上次` 的變更。
- 主控台的主機列表、總覽、告警引擎的 `HostView` 都改讀它，取代「本節點狀態表＋15 秒的清冊快取」的合併（M13）。
- 延遲最多約 2 秒（加上 NOTIFY 時更短）。

**上限**：

- `limits.max_agents`（狀態表）在叢集中是**每節點**的上限。
- `storage.max_hosts`（清冊）是全叢集的上限，以資料庫的筆數判斷；並行時可能略超出（最多節點數筆），可以接受。

### 6.7 跨節點事件（階段 B）

- 原本只在行程內的匯流排事件中，**leader 的告警引擎或主控台需要的事件**另外寫入 `cluster_events`：
  - `host.online`、`host.offline`、`host.added`：SSE 使用；
  - `bus.host_superseded`、`bus.auth_failed`、`bus.registry_evicted`、`bus.host_decommissioned`、`bus.host_rebooted`、`bus.tls_reload`、`bus.condition`：告警引擎使用。
- 每 100 ms 批次寫入，失敗時丟棄並計數（事件本來就只是提示，ALERTS.md 第 1 節）。
- leader 每秒讀取 `id > 上次` 的事件，交給告警引擎的 `on_bus`；SSE 的 `EventHub` 也從這裡取得主機事件。
- `cluster_events.id` 是全叢集遞增的序列，因此 SSE 的事件編號在階段 B 改為 `c<cluster_id 前 8 碼>-<id>`。換節點（B2）或 leader 換手（B1）後，只要事件還在保留範圍內就能接續，不必 `resync`。
- 各節點的本機系統檢查（連線數接近上限、每 IP 上限、寫入失敗、伺服器憑證即將到期）以 `bus.condition` 發布，`subject` 帶 `node_id`。leader 的告警引擎依「檢查項目＋節點」開事件；事件的對象顯示節點名稱（設計包待辦）。

## 7. report 寫入、去重與彙總

### 7.1 寫入路徑

- **每個節點一個 writer task**，group commit 的批次邏輯不變（STORAGE.md 第 4 節）。PostgreSQL 的一批是一個交易：
  - `reports` 以多列 `INSERT … ON CONFLICT DO NOTHING RETURNING` 寫入，取回哪幾列真的寫入、哪幾列是 `Duplicate`；
  - 行程快照、`host_series`、遲到資料的彙總合併在同一個交易。
- **`ack` 的語意不變**：交易 commit 才回 `ack`。PostgreSQL 預設 `synchronous_commit = on`，commit 時 WAL 已同步到主庫的磁碟（比 SQLite 的 `normal` 更強）；有同步備庫時也已送達備庫（第 4.3 節）。
- **結果不明**（commit 時連線中斷、資料庫切換）：與 0.8 的 `Unknown` 相同，不回應並關閉連線，Pulse 重連後重送，由唯一索引去重。PostgreSQL 的錯誤分類：

  | 錯誤 | 結果 |
  |---|---|
  | commit 之前的錯誤 | `Failed` |
  | `40001`、`40P01` | 自動重試，仍失敗時 `Failed` |
  | commit 時連線中斷 | `Unknown` |
  | `53100`（磁碟已滿） | `Failed` |
- **逾時預算不變**：`timeouts.write_secs`（5 秒）＋ 1 秒寬限 ≤ PROTOCOL.md 的 8 秒。PostgreSQL 連線設 `statement_timeout = timeouts.write_secs × 1000 ms`。

### 7.2 去重

- 唯一索引 `reports_dedup (boot_ref, report_seq) WHERE boot_ref IS NOT NULL` 與 `boots (host_id, boot_id)` 原樣搬到 PostgreSQL。因為所有節點共用同一個資料庫，**去重的範圍天然是全叢集的**：Pulse 斷線前在節點 A 送出、尚未收到 `ack` 的 report，在節點 B 重送時，若 A 的交易已 commit 就判為 `Duplicate`。
- **兩個節點同時寫入同一筆**（舊連線的交易仍在進行、新連線已重送）：PostgreSQL 的唯一索引讓後到的那筆等待先到的交易結束；先到的 commit 時後到的得到衝突（`Duplicate`），先到的回復時後到的照常寫入。不會重複，也不會遺失。
- **彙總不重複計算**：只有真正寫入（`Written`）的列才加入彙總（第 7.3 節），`Duplicate` 不加入，與 0.8 相同。
- STORAGE.md 第 5.4 節「`buffer.max_age_hours ≤ 24 × raw_retention_days` 時去重一定有效」的關係不變。

### 7.3 彙總（PostgreSQL 後端）

0.8 的彙總依賴「單一 writer 的記憶體桶」與 `hosts.rollup_until` 的不變式（M17）。多主動時同一台主機的資料可能先後由不同節點寫入，因此 PostgreSQL 後端改為 **leader 從原始資料產生彙總**：

- **寫入端**（任何節點）只寫原始資料，不在記憶體中累積彙總。
- **leader 每 10 秒**對每台主機，取「`rollup_until` 之後、已結束超過 `ROLLUP_SETTLE`（60 秒）的 5 分鐘桶」：
  1. 從 `reports` 依取樣時間讀出這些桶的原始資料（走 `reports_host_ts` 索引）；
  2. 計算 min／max／sum／count，以及計數器速率與推算的使用率（第 7.4 節）；
  3. `INSERT … ON CONFLICT DO UPDATE` 寫入 `rollup_5m` 並合併進 `rollup_1h`；
  4. 推進 `rollup_until`。

  以上在同一個交易中完成（含 fencing）。每步最多約 5,000 列的輸出，只在主機的邊界切開。
- **遲到的資料**（取樣時間早於 `rollup_until`）：寫入端在寫原始資料的同一個交易中直接累加進已寫出的桶，與 0.8 相同。為了避免與 leader 的寫出互相錯過，兩邊以每台主機的 advisory lock 序列化：
  - 寫入端在檢查 `rollup_until` 前取得 `pg_advisory_xact_lock_shared(<主機的鍵>)`；
  - leader 寫出前取得 `pg_advisory_xact_lock(<主機的鍵>)`。

  因此：
  - 寫入端先 commit 時，leader 之後讀原始資料一定看得到它（READ COMMITTED 下每個敘述重新取快照）；
  - leader 先 commit 時，寫入端之後讀到的 `rollup_until` 已推進，會自己合併。
- **補送中的桶**：0.8 以「桶結束 15 秒、且該主機在這個桶 15 秒沒有新值」延後寫出，避免同一個桶被拆成很多次。leader 版改為「桶結束 60 秒」才寫出；之後補送進來的資料走遲到路徑，逐批以 upsert 合併。量測（L1 的 bench）若發現補送尖峰時 upsert 太多，再加回「該主機最近 60 秒有寫入且是補送資料就延後」的規則。
- **重新啟動、換手**：狀態都在資料庫（`rollup_until` 與原始資料），新 leader 直接從 `rollup_until` 繼續，不需要重建記憶體。0.8 的「已知的近似」（原始資料在桶寫出前被保留清除刪除）在 leader 版中仍存在：保留清除也由 leader 執行，清除前先寫出可寫出的桶即可避免，實作時一併處理。
- **SQLite 後端不變**（記憶體桶），因為單機只有一個 writer。兩種演算法的輸出由同一組測試向量確認一致（相同的輸入產生相同的 `rollup_5m`、`rollup_1h`）。

### 7.4 計數器速率與推算值

- **PostgreSQL 後端**：在 leader 產生彙總時，依每台主機、每個計數器序列的取樣時間順序，以相鄰兩筆計算速率（`kairos_proto::counter_rate`）。「上一筆」取 `rollup_until` 之前最近的一筆，與 0.8 重新啟動後的規則相同。比 0.8 的「寫入時以記憶體中的上一筆計算」更正確：不受寫入順序與換節點影響。
- 遲到的資料落在已寫出的桶時不重算速率（0.8 也不產生），速率序列可能在該處留下缺口。與 METRICS.md 的「晚到的資料不產生速率點」一致。

### 7.5 保留期限與容量上限

- **只由 leader 執行**，規則與 STORAGE.md 第 8 節相同（每步 5,000 列一個交易，加 fencing）。
- **使用量**：`storage.max_total_bytes` 改以 Kairos 的資料表與索引的 `pg_total_relation_size` 加總計算（每 60 秒一次，不是每 10 秒，因為查詢成本較高）。不含 WAL 與其他資料庫。
- **檔案不會變小**：與 SQLite 相同，刪除的空間由 autovacuum 回收後重複使用。文件建議 DBA 監看 autovacuum，大量刪除後可在離峰時段執行 `VACUUM (ANALYZE)`。
- **日期保護**（DateGuard）：狀態仍在 `meta.retention_guard`，只有 leader 讀寫。

### 7.6 之後的效能工作（不在本規格的必要範圍）

- `reports` 以取樣時間依日分區（PostgreSQL 宣告式分區），保留期限改為 `DROP` 分區。分區表的唯一索引必須包含分區鍵，而去重鍵 (boot_ref, report_seq) 不含取樣時間；Pulse 重送時 `ts` 不變，但 Kairos 在 `ts` 不合理時改用收到的時間（STORAGE.md 第 5.2 節），重送時收到的時間不同。因此需要另一張不分區的 `report_keys (boot_ref, report_seq)` 表，隨保留期限刪除。
- 單機 SQLite 的量測（1 萬台 × 每 5 秒）在 PostgreSQL 上必須重新量測（L1）：目標是叢集整體每秒 2,000 筆時 `ack` 的 p99 < 50 ms，補送尖峰（每秒 8,000 筆、5 分鐘）沒有 `storage_unavailable`。

## 8. Pulse：多個伺服器位址與 DNS SRV

**原則：有 LB 或虛擬 IP 時 Pulse 完全不需要修改**（`server.address` 指向 LB 的名稱）。本節的功能給沒有 LB、或 LB 本身也要備援（例如兩個機房各一個 LB）的部署使用。

### 8.1 設定

```toml
[server]
address = "kairos-a.example.com"       # 第一優先（不變）
port = 8443
domain = "kairos.example.com"          # 驗證伺服器憑證的名稱（不變；所有位址共用）
# 新增：依序改試的其他位址（"主機" 或 "主機:埠"，省略埠時用 server.port）；最多 16 個
fallback_addresses = ["kairos-b.example.com", "10.0.20.11:8443"]
# 新增（選用）：DNS SRV 名稱。設定時每一輪連線前查詢，結果排在 address 與 fallback_addresses 之前
srv = ""                               # 例 "_kairos-agent._tcp.example.com"
```

- **為什麼用新的鍵，而不是讓 `address` 接受陣列**：
  - **降版安全**：舊版 Pulse 忽略不認得的鍵，降版後照常連 `address`，不會因為設定檔中的陣列而無法啟動。
  - `pulse enroll` 改寫設定檔的工具（`enroll/toml_edit.rs`）只會寫單值，`address` 維持單值就不必改它。
  - 與 Kairos 的 `ldap.fallback_urls` 同樣的命名慣例。
- **環境變數**：`PULSE__SERVER__FALLBACK_ADDRESSES`（與 Pulse 其他環境變數相同的雙底線寫法）以逗號分隔（Pulse 的環境變數目前不支援陣列，這個鍵是第一個例外：逗號分隔、去除前後空白、空字串表示沒有）。
- **驗證**：每個項目依 ENROLL.md 第 14.6 節的端點正規寫法解析（IPv6 必須加 `[]`）；不合法時 `PLS-E0201`（既有代碼，設定值不合法）。`srv` 必須是 `_服務._tcp.網域` 的形式。
- **`server.domain` 不變**：所有候選位址都以同一個名稱驗證伺服器憑證，見第 8.5 節。

### 8.2 候選位址清單

每一輪連線開始時組出候選清單：

1. `srv` 有設定時：查詢 SRV 紀錄，依 RFC 2782 排序（priority 由小到大，同一個 priority 內依 weight 加權隨機）。查詢失敗或沒有紀錄時記 `PLS-E2303`（暫定，每 10 分鐘最多一次），略過這一步。SRV 的 TTL 只用在快取（最多 300 秒）。
2. `address:port`。
3. `fallback_addresses`，依設定順序。

重複的項目只留第一個。每個候選位址的主機名稱在嘗試時才解析（不快取，讓 DNS 的變更立即生效）；解析出多個位址時依序嘗試，每個位址最多 `min(3 秒, 剩餘時間)`，整個候選位址最多 5 秒（沿用 `CONNECT_TIMEOUT`）。這也修正了現在「第一個 IP 沒有回應就用完整個 5 秒」的問題（第 2.4 節）。

### 8.3 選擇與退避

- **一輪**＝從候選清單的第一個開始依序嘗試。
  - 失敗時（DNS、TCP 被拒、逾時、TLS 握手失敗、收到 `hello_ack` 之前斷線或收到錯誤）**立即**改試下一個，兩次嘗試之間只隔 0～500 ms 的隨機延遲。不增加退避，記 `PLS-W2301`（暫定，每輪第一次失敗時記一次，節流 10 分鐘）。
  - 整輪都失敗後才依既有的指數退避（`retry.initial_delay` 加倍到 `retry.max_delay`）等待，下一輪再從第一個開始。
- **伺服器憑證不受信任**（`PLS-E1401`～`E1405`）時同樣改試下一個，但整輪結束時以 `ERROR` 記錄。這通常是部署錯誤（某個節點的憑證不含 `server.domain`），而不是節點故障。
- **連上之後**：退避的重置規則不變（穩定 30 秒）。**不主動切回第一優先**：斷線後下一輪自然從第一個開始。理由：主動切回會在第一優先節點反覆故障時造成來回切換，而階段 A 的備援節點不監聽，切回也沒有意義。
- **被 `superseded`**：規則不變（第 2 次起至少 30 秒），不改試其他位址。原因是身分衝突，換節點也一樣。
- **`storage_unavailable`**：不換位址（連線本身正常，是資料庫的問題），依 durable 的退避規則。
- **排空（`drain`）**：見第 18.2 節，收到後在指定的時間內改連下一個候選位址，不算失敗、不增加退避。

### 8.4 與 durable 補送和去重的互動

- `boot_id`、`report_seq` 與連到哪個節點無關，重送時內容不變。共用資料庫的去重是全叢集的（第 7.2 節），因此換節點補送不會重複，也不會遺失。
- 斷線時在途的 report（最多 4 筆）在下一條連線（可能是另一個節點）從最舊的未確認記錄開始重送，與現在相同。
- **連到不同資料庫的 Kairos**（例如誤把 `fallback_addresses` 指到另一套獨立的 Kairos）：去重失效，可能重複寫入。文件明訂所有候選位址必須屬於同一個叢集；Pulse 以 `hello_ack.server_node` 與 `cluster_id` 記錄連上的叢集（第 18.1 節），`cluster_id` 改變時記 `WARN`。

### 8.5 伺服器憑證的 SAN 與 LB

- Pulse 一律以 `server.domain` 驗證，與實際連上的位址無關。因此**每個節點的伺服器憑證都必須含 `DNS:<server.domain>`**（通常是 LB 或服務的名稱，例如 `kairos.example.com`）。也可以加上各節點自己的名稱（`kairos-a.example.com`），供直接連線的維運工具使用。
- 建議所有節點使用**同一張伺服器憑證**（SAN＝服務名稱＋各節點名稱），由根 CA 離線簽發後分發（PKI.md 第 2 節的伺服器憑證規則不變）。各節點的 `--check-config` 檢查憑證含 `cluster.service_name`（設定時）。
- **LB 只做 TCP 透傳**，TLS 與 mTLS 在 Kairos 上終結（第 3.4 節）。
- 主控台 9443 的憑證同理：瀏覽器連的是服務名稱。

### 8.6 註冊通道（8444）的位址

- **0.9.0 的實作：8444 不跟著候選清單走**，仍只用 `enroll.address`（空字串時為 `server.address` 的主機）＋`enroll.port`；叢集部署請把 `enroll.address` 設為負載平衡器或虛擬 IP 的名稱。
- 0.10.0 預定：`enroll.address` 為空字串（預設）時，註冊端點跟著候選清單走：每個候選位址的主機加上 `enroll.port`。新註冊、輪詢與過期續簽在一個候選位址失敗時改試下一個。`enroll.address` 有設定時只用它（與現在相同）。
- **安裝字串不變**（`pulse-enroll:v1:`，`s` 與 `e` 都是單一端點；ENROLL.md 第 14.7 節不允許未知欄位）。叢集部署的安裝字串一律使用服務名稱（LB 或虛擬 IP）。多位址在註冊後以設定檔或環境變數加入；若之後要讓安裝字串攜帶多個位址，需要 `pulse-enroll:v2:`（第 21 節 #10）。

### 8.7 狀態與可觀測性

- `status_file.rs` 的 `Connection.server` 改為**實際連上的候選位址**（`主機:埠`）。另加三個欄位（schema `pulse.status-file/v1` 不變，只新增欄位）：
  - `server_node`：`hello_ack.server_node`，沒有時為 `null`；
  - `candidates`：候選位址的數量；
  - `candidate_index`：目前連上的是第幾個，從 0 起。
- `pulse status` 顯示「已連線：kairos-b.example.com:8443（節點 kairos-b，第 2 個候選位址）」。所有候選位址都失敗時，列出每個位址最後的錯誤（設計包待辦）。
- `pulse diag` 的設定檢查列出 `fallback_addresses` 與 `srv`，並逐一測試解析與 TCP 連線。遮蔽規則把這兩個鍵加入主機遮蔽名單（與 `server.address` 相同）。
- 本機指標：新增 `pulse_server_candidates`、`pulse_server_candidate_index`、`pulse_connect_failovers_total`（換位址的次數）。不以位址當 label。

### 8.8 SRV 的實作

- 需要能查 SRV 紀錄的解析器。標準函式庫與 tokio 只有 A／AAAA；可選 `hickory-resolver`（MIT 或 Apache-2.0，純 Rust，支援 Windows 讀取系統的 DNS 設定）。它會增加約 10 個依賴，因此 SRV 列為**階段 B 的選用功能**，先做 `fallback_addresses`（第 21 節 #9）。
- 服務名稱建議：agent 連線 `_kairos-agent._tcp`、註冊通道 `_kairos-enroll._tcp`。

## 9. 註冊通道（8444）

| 項目 | 階段 A | 階段 B1 | 階段 B2 |
|---|---|---|---|
| 誰服務 | 主動節點 | leader（`/readyz?scope=enroll`） | 所有節點 |
| 限流 L1～L10 | 不變 | 不變 | 每節點各自計算，文件註明「每節點」；L8（全域申請速率）與 L1（全域連線）的有效值為節點數 × 設定值 |
| L7 封鎖 | 接手時從 `enroll_blocks` 讀回 | 同左 | 每 10 秒依 `cluster_revs.enroll_blocks` 重讀 |
| 撤回的 nonce | 不變 | 不變 | `enroll_nonces` 表 |
| 長輪詢的喚醒 | 不變 | 不變 | 既有的 `notify_external_changes`（每 2 秒查資料庫）已能跨節點；另加 `NOTIFY kairos_rev` 縮短延遲 |
| token 扣次數、重放快取、簽發冪等 | PostgreSQL 的寫法（第 5.4 節 D2～D4） | 同左 | 同左 |
| `observed_ip` | PROXY protocol（第 3.4 節） | 同左 | 同左 |

8443 的 `renew_request` 在階段 B 由收到請求的節點簽發（每個節點都有簽發 CA 私鑰，第 11.2 節），以 D4 的搶占保證冪等。

## 10. 主控台與 API

### 10.1 工作階段與驗證

- 工作階段（`sessions`）、使用者、角色、API token、登入失敗計數（存在的帳號）、TOTP 防重放都已在資料庫中，**跨節點不需修改**（第 5.4 節表末）。
- CSRF 與 TOTP 依賴 `console.key`，所有節點必須是同一把（第 11 節）。
- 工作階段的最後活動時間由服務該請求的節點寫回（最多每 60 秒一次，條件式 UPDATE）。token 的最後使用時間由各節點每 60 秒寫回（`WHERE last_used_at < $new`）。都不需要協調。
- **B1 只由 leader 服務 9443**，因此 M8（驗證失敗限速）、M9（MFA 挑戰）不需處理。B2 再改存資料庫：
  - `login_challenges`：MFA 挑戰，5 分鐘，最多 5 次；
  - `auth_failures`：驗證失敗的 token bucket，每次失敗一個 UPSERT（失敗本來就少）。

  記憶體的 10,000 個來源上限改為資料庫中由 leader 每小時清除。

### 10.2 主控台寫入後「喚醒」其他元件

0.8 的主控台以行程內的呼叫喚醒其他元件，例如核准後喚醒簽發器、撤銷後重新產生 CRL、確認告警時修改告警引擎。叢集中改為：

| 動作 | 0.8 | 叢集 |
|---|---|---|
| 核准申請 | 立即簽發（API 同步） | 收到請求的節點以 D4 搶占後直接簽發（每個節點都有私鑰）；長輪詢由 `NOTIFY` 或 2 秒的輪詢喚醒 |
| 撤銷 | 喚醒 `sync_pki` 產生 CRL | 寫入撤銷並遞增 `cluster_revs.pki`；leader 在 2 秒內（NOTIFY 時更快）產生 CRL；各節點寫出本機檔並重載 TLS |
| 告警的確認、指派、解決、註解 | `AlertService::act()` 直接改記憶體中的引擎 | B1 的主控台就在 leader 上，不變；B2 時非 leader 的節點把動作寫入 `alert_actions`（與 CLI 相同），等待結果最多 10 秒 |
| 規則、靜音、管道的修改 | 寫資料庫並喚醒 | 寫資料庫並遞增 `cluster_revs.alerts_config` |
| 設定（註冊模式、憑證政策） | 寫 `settings` 並 `wake()` | 寫 `settings` 並遞增 `cluster_revs.settings` |

### 10.3 即時推送（SSE）

- **階段 A**：不變。主動節點換手後，瀏覽器以 `Last-Event-ID` 重連到新的主動節點；epoch 不同，收到 `resync("restarted")` 後重新抓取列表，與 Kairos 重新啟動時相同。
- **階段 B**：主機事件改以 `cluster_events` 為來源（第 6.7 節），事件編號為 `c<cluster_id 前 8 碼>-<id>`。
  - leader 換手（B1）或換節點（B2）後，只要 `Last-Event-ID` 的事件仍在 `cluster_events`（保留 1 小時或 10 萬則）就補送遺漏的事件。
  - 超出範圍時 `resync("too_old")`。
  - 告警事件（`alert.*`）由 leader 的告警引擎發布，也寫入 `cluster_events`。
- `resync` 的新原因：`failover`（接手的節點看到的 `Last-Event-ID` 屬於上一個 leader 的 epoch，且無法接續；階段 A 使用，取代 `restarted`，讓主控台可以顯示「伺服器已切換」）。這是 API 的新增值，主控台的處理與 `restarted` 相同（設計包待辦）。
- `console.max_streams` 是每節點的上限。

### 10.4 速率限制在多節點下的行為（總表）

| 限制 | 階段 A | B1 | B2 |
|---|---|---|---|
| report 速率（每 agent_id） | 不變 | 每節點；換節點時取得新桶（多出最多 10 則突發） | 同左 |
| 每連線訊息速率 | 不變 | 不變 | 不變 |
| 8443 每 IP 同時連線、新連線速率 | PROXY 後不變 | 每節點 | 每節點 |
| 8443 全域連線 `server.max_connections` | 不變 | 每節點 | 每節點 |
| 8444 L1～L10 | 不變 | 不變（只有 leader） | 每節點（第 9 節） |
| 主控台驗證失敗 | 不變 | 不變（只有 leader） | 資料庫 |
| 測試通知（每管道 60 秒 3 次） | 不變 | 不變 | leader 執行（測試通知改經 `alert_actions`） |
| 被拒寫入請求的稽核節流（每執行者每分鐘 20 筆） | 不變 | 不變 | 每節點（最多多出節點數倍的稽核紀錄，可以接受） |

## 11. PKI 金鑰與機密

### 11.1 需要一致的機密

| 檔案 | 用途 | 叢集的規則 |
|---|---|---|
| `data/alerts-secrets.key` | 通知管道機密（AES-256-GCM） | 所有節點同一把 |
| `data/console.key` | CSRF、TOTP 秘密加密、復原碼 HMAC | 所有節點同一把 |
| `data/audit-hmac.key` | 稽核鏈的 HMAC | 所有節點同一把 |
| `data/pki/issuing-<ca_id>.key` | 簽發 CA 私鑰（線上簽發） | 所有節點都有啟用中（`active`）與 `trusted` 狀態的每一把 |
| 伺服器憑證與私鑰（`server.cert_path`、`console.cert_path`） | TLS | 各節點各自一份，SAN 規則見第 8.5 節（建議同一張） |
| `observability.metrics_token_file` | `/metrics` 的 token | 建議一致（Prometheus 設定較簡單），不檢查 |
| `storage.postgres` 的密碼檔 | 連線資料庫 | 各節點各自一份 |

**指紋與檢查**：

- `kairos cluster init` 時計算前四類金鑰的指紋，寫入 `meta.cluster_keys`：
  - 對稱金鑰：`HMAC-SHA256(金鑰, "kairos-key-fingerprint/v1\n" ‖ 檔名)` 的前 16 bytes；
  - 簽發 CA 私鑰：公鑰的 SPKI SHA-256，與 `pki_cas` 中的憑證比對。
- 每個節點啟動時計算自己的指紋並與資料庫比對：
  - **缺少或不符**時記 `KRS-E8404`，`/readyz` 回 503（`keys` 檢查失敗），**不接受連線、不參與 leader 選舉**；
  - 行程照常執行（`/healthz` 200），讓維運者看得到原因。
- **叢集模式下停用所有金鑰的自動建立**：`SecretBox::open`、`ConsoleKey::load_or_create`、`audit::ensure_key` 在 `cluster.enabled = true` 時只讀取、不建立。只有 `kairos cluster init`（第一個節點）會在金鑰不存在時建立。理由：0.8 在金鑰遺失時自動建立新金鑰，在單機上只是「舊的密文解不開」；在叢集中會讓節點之間在無聲無息中分岔，而且新金鑰寫入的資料其他節點讀不了。

### 11.2 簽發 CA 私鑰

- **建議：每個節點都放同一組簽發 CA 私鑰**（第 21 節 #6）。理由：
  - 階段 A 的備援節點接手時必須能簽發；
  - 8444 的自動核准與過期續簽、8443 的 `renew_request` 都是同步簽發，受 5 秒處理時限約束，轉送到 leader 會增加延遲與失敗模式；
  - 金鑰的保護等級不變（每個節點都是 0600、擁有者 `kairos`、PKI.md 第 3 節的權限檢查照舊），只是份數變多。
- `kairos pki issuing-ca new` 在任一節點執行，產生的私鑰檔要以 `kairos cluster keys export`／`import` 分發到其他節點，之後才能 `install` 與 `activate`。`activate` 前檢查所有線上節點都回報有這把金鑰（`cluster_nodes.info` 中的金鑰指紋清單）；不齊時拒絕並列出缺少的節點（`KRS-E8404`），可以 `--force` 略過（例如節點已確定要移除）。
- 離線簽發模式（`pki.mode = "offline"`）不受影響：沒有線上私鑰。
- 之後可考慮的替代方案（不在本規格）：
  - 私鑰以 KEK 加密存在資料庫，KEK 由 systemd `LoadCredentialEncrypted=` 或 TPM 提供；
  - 外部 KMS 或 HSM（PKCS#11）。

### 11.3 CRL 與信任錨

- CRL 只由 leader 產生（D5），寫入 `pki_crls` 並遞增 `cluster_revs.pki`。
- **每個節點**的 `sync_pki`（每 2 秒，或收到 `NOTIFY` 時）從資料庫讀出目前的信任錨與 CRL，內容不同時寫出本機的 `data/pki/issuing-cas.pem`、`crl/issuing.crl`、`issuing-cas-retired.pem`，並觸發本機的 TLS 重載（既有的 `reload::trigger()`）。重載後照常重新驗證所有現役連線，被撤銷的連線收到 `certificate_rejected`。
- 撤銷生效到所有節點拒絕的延遲：leader 產生 CRL（≤ 2 秒）＋各節點寫出並重載（≤ 2 秒），合計約 4 秒（0.8 單機約 2 秒）。
- CRL 過期的保護不變：fail closed（`KRS-E1203`）。leader 長時間不存在時 CRL 不會更新，`nextUpdate` 剩 7 天起照常 `KRS-W1201`；系統健康的 `cluster` 項目另外顯示「沒有 leader」（`KRS-E8403`）。

### 11.4 金鑰的分發工具

```bash
# 第一個節點：建立叢集（產生 cluster_id、記錄金鑰指紋；金鑰不存在時建立）
sudo -u kairos kairos cluster init --config /opt/kairos/kairos.toml
# 匯出所有需要一致的金鑰（含簽發 CA 私鑰），以備份密碼加密
sudo -u kairos kairos cluster keys export /root/kairos-cluster.keys --password-stdin --config … < pass
# 其他節點：匯入（寫入本機資料目錄，權限 0600；已存在且相同時不變，不同時拒絕，除非 --overwrite）
sudo -u kairos kairos cluster keys import /root/kairos-cluster.keys --password-stdin --config … < pass
```

- 格式 `kairos.keys-backup/v2`：與 v1（STORAGE.md 第 9 節，Argon2id＋AES-256-GCM）相同的 KDF 與加密，內容另含 `pki/issuing-<ca_id>.key`。`kairos db restore-keys` 同時接受 v1 與 v2。
- 匯入後立即重新計算指紋並與資料庫比對，結果寫在輸出中。

## 12. 稽核雜湊鏈

### 12.1 多寫入者下的一致性

0.8 的正確性來自「先 INSERT 取得 SQLite 的寫入鎖，再以 `id < ?` 找前一筆」。PostgreSQL 版改為：

```text
audit_append(tx, event):                                 -- 與被記錄的變更在同一個交易，一律是最後一步
  head = SELECT value FROM meta WHERE key = 'audit_chain_head' FOR UPDATE   -- 全叢集序列化
  id   = head.id + 1                                     -- 持鎖期間指定，不用序列
  hash = SHA-256(head.hash ‖ "\n" ‖ JSON([id, at, actor, …]))
  hmac = HMAC(K, "kairos-audit-record/v1\n" ‖ hash)      -- 起點之後
  INSERT INTO audit_events (id, …, prev_hash, hash, hmac) VALUES (…)
  UPDATE meta SET value = {id, hash, mac} WHERE key = 'audit_chain_head'
```

- **`meta.audit_chain_head` 的列鎖序列化所有寫入者**：各節點的服務、主控台、所有 CLI。鎖持有到交易結束，因此鏈的順序等於 commit 的順序，`id` 連續、沒有空號。回復的交易不佔用 `id`。
- **鎖的順序**：稽核一律是交易的最後一步（第 5.4 節），持鎖時間是「計算雜湊＋兩個敘述」（約 1 ms）。長交易（例如批次核准）要先完成業務寫入再呼叫 `audit_append`，否則會擋住其他寫入者的稽核。
- **head 不存在時**（空資料庫、或 0.5 的未封存紀錄）：沿用 0.8 的 `seal_legacy` 規則。`seal_legacy` 也要在持有 head 鎖時執行。
- **驗證**：在 `REPEATABLE READ` 的唯讀交易（同一個快照）中執行，與 0.8「同一個讀取交易」的保證相同。
- **單機 SQLite**：照舊（寫入鎖已經序列化）。兩個後端計算出的 `hash` 與 `hmac` 完全相同，`kairos db migrate` 搬移後可以直接驗證（第 5.6 節）。
- **量**：稽核紀錄每秒最多數十筆（登入、管理操作），序列化不是瓶頸。被拒寫入請求的節流（每執行者每分鐘 20 筆）在 B2 為每節點（第 10.4 節）。

### 12.2 HMAC 金鑰與自我驗證

- 所有節點使用同一把 `audit-hmac.key`（第 11.1 節）。0.8 的 `sign_one` 在 `key_id` 不符時不簽，紀錄會變成 `missing_hmac`；叢集的指紋檢查在啟動時就擋住這種節點，不會產生沒有 HMAC 的紀錄。
- 自我驗證（每 `audit.verify_interval_hours`）、鏈尾外送（每 `audit.anchor_log_minutes`）、保留清除只由 leader 執行（第 6.3 節），因此每個週期只有一筆 `audit.verify`、一行 `audit.chain_anchor`。
- 鏈尾外送的日誌行另帶 `node_id`。SIEM 收到的鏈尾在換手前後來自不同節點，但 `last_id` 與 `last_hash` 是同一條鏈。
- `meta.audit_hmac` 的起點不變；`kairos cluster init` 不改變起點。

## 13. 告警引擎

### 13.1 只由 leader 執行

- 告警服務（判定、通知、動作佇列、清除、心跳）是 leader 的工作（第 6.3 節）。
- 所有寫入交易加 fencing。換手時舊 leader 正在寫的事件要嘛已 commit（新 leader 載入），要嘛失敗（新 leader 依目前的條件重新判定）。
- 新 leader 啟動告警服務時，與 0.8 啟動時相同：
  - 從資料庫載入未解決的事件、靜音、規則、管道；
  - 事件流水號改用 `alert_event_seq`（D7），不再依賴記憶體中的 `seq_by_day`。
- `streaks`（for 期間的計時）在記憶體中，換手後重新計時。文件註明：「換手後，條件已成立但尚未達到 `for` 期間的規則會重新計時」。

### 13.2 停機與換手的判斷（`alerts_heartbeat`）

- `meta.alerts_heartbeat` 改為**叢集層級的 leader 心跳**：leader 每 30 秒寫入 `{at, node_id, epoch, clean}`。
- 新 leader 啟動告警服務時讀取它：

  | 情況 | 處理 |
  |---|---|
  | 上一個 leader 是自己、或沒有叢集（單機） | 與 0.8 相同：「Kairos 已重新啟動」，停機超過 5 分鐘另開「停機 N 分鐘」 |
  | 上一個 leader 是另一個節點，且心跳在 `lease_ttl_secs + 60 秒` 內 | **換手**：記一則資訊事件「叢集的 leader 已由 kairos-a 切換到 kairos-b」（`KRS-I8401`）。上一個 leader 不是 `clean` 時為警告。不開「停機」事件 |
  | 上一個 leader 是另一個節點，且心跳更舊 | 「叢集沒有 leader 達 N 分鐘」：嚴重，直接為已恢復、需要確認（取代「停機 N 分鐘」；`KRS-E8403`）。期間 agent 連線仍可能正常（階段 B），但告警、彙總、CRL 暫停 |
- **暖機期**：
  - **階段 A 的換手**：所有 agent 都要重連，等同重新啟動，**照常套用暖機期**（`alerts.warmup_secs`）。
  - **階段 B 的換手**：agent 連線不受影響，**不套用暖機期**（`expected` 主機清單取自 `host_presence`，而不是上一次的心跳）。

### 13.3 通知不重複、不遺失

- 通知是 **at-least-once**：
  - leader 在送出前把 `alert_deliveries.claim_epoch` 設為自己的 epoch（fencing 交易）；
  - 送出後在另一個交易寫回結果。
  - 送出之後、寫回之前 leader 失效時，新 leader 看到 `state = 'pending'` 且 `claim_epoch` 是舊 epoch，會再送一次。
- 為了讓接收端去重，Webhook 的 `kairos.alert.v1` 新增 `delivery_id` 欄位（同一則通知重送時不變；格式的新增欄位，接收端可以忽略）。Email 加 `X-Kairos-Delivery-Id` 標頭。設計包待辦（第 22 節）。
- 每管道 30 秒的速率限制、連續失敗計數在 leader 的記憶體中，換手後重新計算。最壞情況是換手後的第一個 30 秒內多送一則合併通知。

### 13.4 階段 B 的主機狀態來源

- 0.8 的 `world_hosts()` 合併「資料庫的清冊」與「本機狀態表的 `live_view()`」。階段 B 改為 `ClusterView`（第 6.6 節）：任何節點上的連線都看得到，不會把連在其他節點的主機誤判為離線。
- 匯流排事件經 `cluster_events` 送到 leader（第 6.7 節）。
- 新的系統規則（設計包待辦，嚴重度為建議）：

  | 規則 | 嚴重度 | 代碼 |
  |---|---|---|
  | `sys.cluster_node_lost` 叢集節點失聯 | 警告 | `KRS-W8406` |
  | `sys.cluster_keys_mismatch` 節點的金鑰與叢集不一致 | 嚴重 | `KRS-E8404` |
  | `sys.cluster_config_drift` 節點的設定不一致 | 警告 | `KRS-W8405` |
  | `sys.db_async_replication` 資料庫沒有同步複寫 | 規劃提醒 | `KRS-W4403` |
  | `sys.cluster_clock_skew` 節點與資料庫的時差過大 | 警告 | `KRS-W8305` |

## 14. 可觀測性

### 14.1 `/healthz` 與 `/readyz`

- **`/healthz`**：不變（存活；備援節點也是 200）。
- **`/readyz`**：`checks` 新增三項：

  | 檢查 | 值 |
  |---|---|
  | `cluster` | `leader`、`standby`、`member`、`lost`（連不上資料庫或租約狀態不明）；單機時為 `disabled` |
  | `keys` | `ok`、`mismatch` |
  | `schema` | `ok`、`newer`、`older` |

  本文同樣只有狀態字，不含節點名稱與位址。
- **`scope` 參數**：

  | `scope` | 200 的條件 | 用途 |
  |---|---|---|
  | （無） | 階段 A：本節點是 leader 且其他檢查都通過。階段 B：本節點可以服務 agent 連線（`cluster` 為 `leader` 或 `member`） | 預設 |
  | `agent` | 同上 | LB 的 8443 後端 |
  | `enroll` | 階段 A、B1：leader，且註冊端點在監聽。B2：任何就緒的節點 | LB 的 8444 後端 |
  | `console` | 階段 A、B1：leader。B2：任何就緒的節點 | LB 的 9443 後端 |
  | `leader` | 本節點是 leader | keepalived 的 `track_script` |
- `database` 檢查改為「連線池可以取得連線並執行 `SELECT 1`」（5 秒快取），不再以讀寫模式開檔。

### 14.2 系統健康（`GET /api/v1/system/health`）

新增項目 `cluster`（需要 `health:read`；細節需要 `settings:read`）：

- **`values`**：
  - `enabled`、`mode`（`active_passive`／`active_active`）、`cluster_id`；
  - `leader`（`node_id`、`epoch`、`since`）；
  - `nodes[]`（`node_id`、`role`、`version`、`heartbeat_age_ms`、`connections`、`config_drift`、`keys_ok`）；
  - `database`（`backend`、`server_version`、`synchronous_replication`、`clock_skew_ms`）。
- **狀態**：

  | 狀態 | 條件 |
  |---|---|
  | 警告 | 有節點失聯（`KRS-W8406`）、設定不一致（`KRS-W8405`）、沒有同步複寫（`KRS-W4403`）、時差過大（`KRS-W8305`）、只有一個節點（沒有備援） |
  | 嚴重 | 沒有 leader（`KRS-E8403`）、有節點的金鑰不一致（`KRS-E8404`） |

其他項目（`connections`、`report_writes`、`tls`、`listeners`、`file_descriptors`）是**回應請求的節點**的值，`values` 另帶 `node_id`。B1 的主控台在 leader 上，這些值是 leader 自己的；其他節點的值由 `nodes[].info` 摘要提供。

### 14.3 `GET /api/v1/cluster` 與 CLI

- **`GET /api/v1/cluster`**（`settings:read`）：同上的 `values`，另加每個節點的監聽位址與最近的錯誤。
- **`POST /api/v1/cluster/nodes/{node_id}/drain`**、**`POST /api/v1/cluster/step-down`**：需要新權限 `cluster:write`（只有管理員），寫稽核（類別 `cluster`）。
- **CLI**：

  | 命令 | 用途 |
  |---|---|
  | `kairos cluster status [--json]` | 讀資料庫，不需要服務在本機執行；`schema` 為 `kairos.cluster.status/v1` |
  | `kairos cluster init` | 建立叢集（第 11.4 節） |
  | `kairos cluster keys export\|import` | 分發金鑰（第 11.4 節） |
  | `kairos cluster drain <node_id>` | 排空節點（第 18.2 節） |
  | `kairos cluster step-down` | leader 主動交還租約 |
  | `kairos cluster forget <node_id>` | 刪除已停用節點的紀錄 |

### 14.4 `/metrics` 新增

| metric | 說明 |
|---|---|
| `kairos_cluster_enabled` | 0／1 |
| `kairos_cluster_role{role}` | 本節點的角色（`leader`、`standby`、`member`、`draining`），值為 0／1 |
| `kairos_cluster_lease_epoch` | 目前看到的 leader epoch |
| `kairos_cluster_lease_renewals_total{result}` | 續約或嘗試取得的次數（`ok`、`lost`、`error`） |
| `kairos_cluster_leader_changes_total` | 本節點看到的換手次數 |
| `kairos_cluster_nodes{state}` | 叢集節點數（`alive`、`lost`） |
| `kairos_cluster_kicks_total{direction}` | 跨節點踢除（`sent`、`received`） |
| `kairos_cluster_events_lag_seconds` | 讀取 `cluster_events` 的落後 |
| `kairos_db_pool_connections{state}` | 連線池（`idle`、`busy`） |
| `kairos_db_transaction_retries_total{reason}` | `40001`、`40P01` 等自動重試 |
| `kairos_db_clock_skew_seconds` | 節點與資料庫的時差 |

Prometheus 以 `instance` 區分節點；`kairos_agents_online` 等既有的計數是每節點的值，全叢集以 `sum` 計算。`packaging/kairos-alerts.example.yml` 新增範例規則：沒有 leader、節點失聯、續約失敗增加。

### 14.5 日誌

- 每一行加 `node`（文字格式在主機名稱之後；JSON 格式為 `node_id` 欄位），只在叢集模式下輸出。
- 新事件：`cluster.leader_acquired`（`KRS-I8401`）、`cluster.leader_lost`（`KRS-W8402`）、`cluster.node_lost`（`KRS-W8406`）、`cluster.kick`、`cluster.drain`。

## 15. 設定

### 15.1 新增的設定

```toml
[cluster]
enabled = false                 # true：叢集模式（需要 storage.backend = "postgres"）
mode = "active_passive"         # active_passive（階段 A）／active_active（階段 B）
node_id = ""                    # 空字串：主機名稱（小寫）。叢集內唯一，1～63 個字元 [a-z0-9-]
service_name = ""               # 選用：服務名稱（例 kairos.example.com），--check-config 檢查伺服器憑證含這個 SAN
advertise = ""                  # 選用：顯示用的位址
lease_ttl_secs = 15             # 5..=120；應大於資料庫自動切換所需的時間
heartbeat_secs = 5              # 1..=30
node_lost_secs = 20             # >= 3 × heartbeat_secs

[storage]
backend = "postgres"            # sqlite（預設）／postgres／jsonl

[storage.postgres]
host = "db.example.com"         # 或 Unix socket 目錄
port = 5432
dbname = "kairos"
user = "kairos"
password_file = "/etc/kairos/pg.password"   # 0600；不支援在設定檔中寫明文密碼
schema = "kairos"
sslmode = "verify-full"         # verify-full（預設）／require／disable
ca_file = "/etc/kairos/pg-ca.pem"
pool_size = 16                  # 每節點，2..=128
connect_timeout_secs = 5

[server]
proxy_protocol_from = []        # 信任的 LB 位址（CIDR）；來自這些位址的連線必須帶 PROXY protocol v2
# enroll.proxy_protocol_from、console.proxy_protocol_from：未設定時沿用 server 的值
```

規則：

- `cluster.enabled = true` 而 `storage.backend` 不是 `postgres`：拒絕啟動，`KRS-E0202`（設定互相矛盾）。
- `storage.backend = "postgres"` 而 `cluster.enabled = false`：允許。單一節點使用 PostgreSQL，leader 恆為自己，不寫租約。
- `jsonl_mirror = true` 與叢集同時設定：拒絕啟動，`KRS-E0202`。
- PostgreSQL 的密碼只從檔案讀取，與 `ldap.bind_password_file` 的慣例相同；`kairos diag` 的遮蔽規則已涵蓋名稱含 `password` 的鍵。

### 15.2 設定的一致性

- 叢集中的節點必須使用**相同的設定**，下列「節點本機」的鍵除外：
  - `cluster.node_id`、`cluster.advertise`；
  - 所有 `*.listen`、`observability.listen`；
  - `storage.data_dir`、`log.*`；
  - 各憑證與私鑰的路徑（`*.cert_path`、`*.key_path`、`storage.postgres.ca_file`、`password_file`）。
- 節點啟動時計算其餘設定的正規化雜湊（`config_hash`），寫入 `cluster_nodes`。與 leader 不同時記 `KRS-W8405`（警告，不拒絕；滾動修改設定的期間本來就會不一致），系統健康顯示哪些節點不同。
- `kairos config show --effective` 多一欄顯示這個鍵是否屬於節點本機。

## 16. 升級路徑、備份與還原

### 16.1 從單機 0.8 升級到 HA

1. **升級到 0.9（單機）**：
   - 依 CHANGELOG 的一般步驟升級，schema 自動升到 v9；
   - 叢集用的資料表保持空白，行為與 0.8 相同；
   - 建議運作一段時間確認穩定。
2. **準備 PostgreSQL**：
   - 建立資料庫與帳號，設定同步複寫（第 4.3 節）；
   - 在 Kairos 主機放好 `password_file` 與 `ca_file`；
   - 以 `kairos --check-config` 檢查連線（新增 PostgreSQL 的檢查：版本、編碼、權限、同步複寫）。
3. **停機搬移**：

   ```bash
   sudo systemctl stop kairos
   sudo -u kairos kairos db backup /var/backups/kairos/pre-ha.db --include-keys --password-stdin --config … < pass
   sudo -u kairos kairos db migrate --to postgres --config /opt/kairos/kairos.toml [--reports-since 2d]
   ```

   停機期間 Pulse 0.3 以上把取樣存在本機緩衝，恢復後補送。
4. **改設定、建立叢集、啟動第一個節點**：

   ```bash
   # kairos.toml：storage.backend = "postgres"、[storage.postgres]、[cluster] enabled = true、node_id = "kairos-a"
   sudo -u kairos kairos cluster init --config /opt/kairos/kairos.toml
   sudo systemctl start kairos     # 取得租約，成為 leader
   ```

5. **加入第二個節點**：
   - 安裝同版本的 Kairos，複製設定（改 `node_id`）與伺服器憑證；
   - 以 `kairos cluster keys export`／`import` 複製金鑰；
   - 啟動。`kairos cluster status` 應顯示一個 leader、一個 standby。
6. **切換入口**：
   - 設定 LB 或 keepalived；
   - 把原本的 Kairos DNS 名稱（Pulse 的 `server.address`、`server.domain`）指到 LB 或虛擬 IP。
   - **Pulse 不需要任何修改**，前提是伺服器憑證含原本的名稱。原本 Pulse 直接以 IP 連線時，把該 IP 改為虛擬 IP。
7. **驗證故障切換**：`kairos cluster step-down` 或停止 leader，確認另一個節點在預期時間內接手、Pulse 重連、資料不缺號（第 17 節）。

**回到單機**：

```bash
# 停止所有節點後
kairos db migrate --to sqlite:/opt/kairos/data/kairos.db
# 改回 backend = "sqlite"、cluster.enabled = false 後啟動
```

### 16.2 叢集中的版本升級

- **schema 不變的版本**（PATCH）可以**滾動升級**：
  1. 排空備援或 member 節點（`kairos cluster drain`）並升級；
  2. `step-down` 讓升級好的節點成為 leader；
  3. 再升級其餘節點。

  叢集允許暫時混用兩個版本，`cluster_nodes.version` 不同時系統健康顯示提示。
- **schema 改變的版本**：**全部停止 → 升級所有節點的執行檔 → 啟動第一個節點**（執行遷移）**→ 啟動其餘節點**。版本較舊的節點看到較新的 schema 時拒絕啟動（`KRS-E4405`）。停機時間約等於遷移時間加上啟動時間。之後若要支援不停機的 schema 遷移（expand／contract），需要另外的規格（第 21 節 #18）。

### 16.3 備份與還原

- **資料庫**：使用 PostgreSQL 的標準工具：
  - `pg_dump`（邏輯備份，適合中小型）；
  - pgBackRest 或 `pg_basebackup` 加 WAL 歸檔（實體備份與 PITR，建議）。

  `kairos db backup` 在 PostgreSQL 後端時拒絕並提示改用上述工具，或以 `kairos db migrate --to sqlite:<檔案>` 產生一個可攜的 SQLite 快照（需要停止所有節點）。
- **金鑰**：不在資料庫中，以 `kairos cluster keys export` 另外加密備份。還原時金鑰與資料庫要是同一個時間點的組合，規則與 STORAGE.md 第 9 節相同。
- **還原**：
  1. 停止所有節點；
  2. 以 PostgreSQL 的工具還原資料庫；
  3. 確認各節點的金鑰指紋與 `meta.cluster_keys` 相符；
  4. 啟動第一個節點，再啟動其餘節點。

  還原到較早的時間點時，該時間點之後 Pulse 已收到 `ack` 的資料不在資料庫中，也不會補送（Pulse 已刪除），與單機還原相同。
- **稽核鏈**：還原後第一次自我驗證會發現 `head_mismatch`（外送的鏈尾比資料庫新）。這是預期的；文件說明以外送的鏈尾確認還原點。

## 17. 故障情境與預期行為

預設參數：`lease_ttl_secs = 15`、Pulse 心跳 15 秒（讀取閒置 30 秒）、退避 2 秒起。

| # | 情境 | 階段 A 的預期行為 | 階段 B 的預期行為 | 資料 |
|---|---|---|---|---|
| F1 | leader 行程當掉（主機仍在） | TCP 被重設，Pulse 立即斷線並開始重連（連不上時依退避重試）；備援在 ≤ 17 秒取得租約並開始監聽；Pulse 約 25～35 秒內連上 | 該節點的 agent 立即改連其他節點（LB 移除它，或 Pulse 改試下一個位址），約 5～10 秒；leader 的工作在 ≤ 17 秒內由其他節點接手 | Pulse 緩衝補送；在途的 report 重送並去重；不遺失、不重複 |
| F2 | leader 主機斷電、網路線被拔 | Pulse 等讀取閒置逾時（30 秒）才發現；總計約 45～60 秒 | 同左，但只影響該節點的 agent | 同上 |
| F3 | leader 與資料庫之間的網路中斷（其他節點正常） | leader 的寫入失敗（`storage_unavailable`），10 秒內自我降級並結束；備援接手 | leader 降為 member；它的 agent 收到 `storage_unavailable`、緩衝後重送；`/readyz?scope=agent` 回 503，LB 把新連線導向其他節點。Kairos 不主動關閉這些連線（避免網路抖動時大量重連），agent 依 durable 退避持續重送 | 不遺失（`storage_unavailable` 代表確定未保存） |
| F4 | 備援節點（或 member）當掉 | 沒有影響；系統健康警告「節點失聯」 | 該節點的 agent 改連其他節點；leader 在 20 秒後把它的主機標為 `server_lost` | 同 F1 |
| F5 | PostgreSQL 主庫故障、自動切換到備庫（約 10～30 秒） | 寫入失敗或結果不明，Pulse 依規則重送；切換時間大於 `lease_ttl_secs − 5` 秒時 leader 降級並結束，切換完成後由某個節點重新取得租約。建議把 TTL 設得比資料庫的切換時間長 | 同左；agent 連線保留，切換完成後恢復寫入 | 同步複寫時不遺失；非同步時可能遺失主庫上最後幾個已 `ack` 的交易（`KRS-W4403`） |
| F6 | PostgreSQL 完全停止 | 沒有節點能持有租約；主動節點結束，所有節點都在等待；Pulse 連不上、緩衝資料（預設最多 168 小時） | 所有節點回 `storage_unavailable`、`/readyz` 503；告警、彙總暫停 | 不遺失（在 Pulse 緩衝內） |
| F7 | LB 故障（或虛擬 IP 的主機故障） | 有 `fallback_addresses` 的 Pulse 改連節點的直接位址（必須是服務中的那個節點，因為階段 A 的備援不監聽）；沒有的等 LB 恢復。keepalived 的虛擬 IP 會移到另一台 | 同左；直接位址都可以服務 | 同上 |
| F8 | 節點之間的網路中斷，但都連得到資料庫 | 沒有影響（資料庫是唯一的仲裁者） | 沒有影響 | — |
| F9 | 腦裂嘗試：舊 leader 暫停（GC、虛擬機凍結）超過 TTL 後恢復 | 舊 leader 恢復後的第一個 leader 專屬交易因 fencing 失敗，隨即自我降級並結束。凍結前已接受、尚未關閉的 agent 連線寫入的 report 照常保存（durable 去重），不影響正確性 | 舊 leader 的 leader 專屬交易失敗並降級；它的 agent 連線不受影響 | 不重複、不覆寫 |
| F10 | 節點的時鐘偏差 | 租約用資料庫的時間，不受影響；偏差超過 2 秒記 `KRS-W8305`。資料的 `last_seen`、`received_at` 用節點時間，偏差會反映在主控台上 | 同左；`host_presence.last_seen` 由各節點寫入，偏差大時離線判斷可能提早或延後（leader 判斷節點失聯用的是資料庫時間） | — |
| F11 | 新節點的金鑰不一致或缺少 | 該節點 `/readyz` 503（`keys: mismatch`）、不參與選舉、記 `KRS-E8404`；系統健康嚴重 | 同左，不接受連線 | 不產生無法解開的資料 |
| F12 | 節點的設定不一致 | 記 `KRS-W8405`；照常運作 | 同左 | — |
| F13 | 兩台主機誤用同一個 `node_id` | 後啟動的拒絕啟動（`KRS-E8403`） | 同左 | — |
| F14 | 滾動升級、排空（drain） | 排空備援：沒有影響。排空 leader：先 step-down，備援接手（等同 F1，但沒有等待 TTL，約 5～10 秒） | 被排空的節點停止接受新連線、`/readyz` 503，對協商了 `cluster` 的 Pulse 分批送 `drain`（第 18.2 節），其他 Pulse 在 60 秒內分批關閉連線 | 不遺失 |
| F15 | 在任一節點上執行 CLI（`kairos enroll approve`、`alerts ack` 等） | CLI 直接連資料庫，與節點的角色無關；`alerts ack` 等經 `alert_actions` 交給 leader | 同左 | 稽核鏈經 head 列鎖序列化 |
| F16 | 所有節點都停止 | Pulse 緩衝資料；恢復後補送 | 同左 | 在緩衝範圍內不遺失 |
| F17 | 同一個 agent 在兩個節點上同時連線（例如 LB 換手後舊 TCP 尚未斷） | 不會發生（只有一個節點監聽） | 後到的 hello 踢掉先到的（第 6.5 節） | 不重複 |

## 18. 協定的增補（草稿，協定版本不變）

本節在實作時併入 PROTOCOL.md 的「版本協商」、「訊息」與「重連」各節，並加入 `kairos-proto` 的型別與測試向量。

### 18.1 feature `cluster`

- Pulse 支援本節的功能時在 `hello.features` 宣告 `cluster`（Pulse 0.9 一律宣告）。Kairos 在 **client 也宣告時**就在 `hello_ack.features` 列出——**單機的 Kairos 0.9 也宣告**（帶節點名稱、關機時排空），不限叢集模式（沿用 `renew` 的先例：沒有宣告的 client 收到的 `hello_ack` 與 0.8 完全相同）。
- 協商了 `cluster` 時，`hello_ack` 另帶：
  - `server_node`（string，`cluster.node_id`，1～63 bytes；單機時預設為主機名稱的第一段）；
  - `cluster_id`（string，UUID；只在叢集模式送，單機的 Kairos 不送）。

  Pulse 寫入狀態檔與日誌，用來診斷「連上哪個節點」與「是不是同一個叢集」。型別不符時視為沒有。
- 協商了 `cluster` 時，Kairos 可以送 `drain`（第 18.2 節）。

### 18.2 排空訊息 `drain`（Kairos → Pulse）

| 欄位 | 型別 | 說明 |
|---|---|---|
| `type` | `"drain"` | |
| `seq` | `null` | 不回應任何訊息 |
| `reason` | string | `node_draining`（維運排空）、`rebalance`（之後的負載重新分配，v9 不送） |
| `reconnect_within_ms` | u32 | Pulse 在 `[0, reconnect_within_ms]` 間隨機選一個延遲後重連，1,000～300,000。Kairos 依被排空的連線數設定，預設 `max(10,000, 連線數 × 5)` ms，避免同時湧向其他節點 |

**Pulse 收到後**：

1. 停止送出新的 report（取樣照常寫入本機緩衝）；
2. 等待在途的 report 收到回應（最多 8 秒）；
3. 送出 TLS close_notify 並關閉連線；
4. 依延遲重連，從候選清單中**目前位址的下一個**開始（只有一個位址時就是同一個，由 LB 導向其他節點）。

這次斷線不算失敗、不增加退避、不重置「連續被 `superseded`」以外的計數，記 `PLS-I8407`（暫定，資訊）。

**Kairos 端**：

1. `kairos cluster drain <node_id>` 把節點的 `role` 改為 `draining`；
2. 該節點的 `/readyz` 回 503、停止接受新連線；
3. 對已連線的 agent 分批送 `drain`：協商了 `cluster` 的送 `drain`，其他的直接關閉連線（原因 `server_shutdown`，舊版 Pulse 依退避重連，最初的延遲約 2 秒加上隨機分散）；
4. 所有連線都關閉後節點回報 `drained`，可以安全停止。

記 `KRS-I8407`（暫定）。

**0.9.0 的實作（單機）**：還沒有 `kairos cluster drain`；Kairos 在**優雅關機**時先對協商了 `cluster` 的連線送 `drain`（`reason`＝`node_draining`，`reconnect_within_ms`＝`max(10000, 線上連線數 × 5)`）再 `close_notify`，其他連線照舊直接關閉，關機日誌記 `KRS-I8407`。

**沒有協商 `cluster` 時**，Kairos 不送 `drain`。舊版 Pulse 收到不認得的 `type` 只會記 DEBUG 並忽略（第 2.4 節），所以即使誤送也不會壞，但規格上不送。

### 18.3 不變的部分

- 協定版本 v1～v2、`hello`、`report`、`ack`、`error` 的格式與錯誤碼都不變。**不新增錯誤碼**：資料庫無法寫入沿用 `storage_unavailable`，跨節點踢除沿用 `superseded`。
- 「伺服器回應延遲最多 8 秒」的預算不變（PostgreSQL 的 `statement_timeout` 與 `timeouts.write_secs` 相同）。
- `hello_ack.offline_after_ms` 的計算不變。
- 8444 的協定（ENROLL.md）不變。`server-info` 的 `features` **0.9.0 暫不加入** `cluster`（8444 還不跟著候選清單走，第 8.6 節）；0.10.0 再與 `hello_ack.features` 對齊。

## 19. 暫定錯誤代碼

依 `copywriting.md` §3.1 的格式（數字本身唯一、鏡像同號）。下表的號碼都是**暫定**，正式分配由設計包負責（第 22 節）。

選號時核對過 `copywriting.md` §3.3 的總表（2026-10-16，0.8.0）：子類別 23、44、84 完全沒有使用；2206、8305 未使用。衝刺 8 的其他規格線（`sprint8/export`、`sprint8/privacy`、`sprint8/remote-mgmt-spec`）在核對時尚未新增代碼，**合併前需要再核對一次**。

新增子類別（建議寫入 §3.2）：

- **23**：多伺服器位址與叢集連線（類別 2 連線與協定）；
- **44**：外部資料庫與資料遷移（類別 4 儲存與資料）；
- **84**：叢集與高可用（類別 8 平台與服務）。

| 號碼 | Kairos | Pulse | 兩端 | 情況 | 處理方式（摘要） |
|---|---|---|---|---|---|
| 2206 | `KRS-W2206` | | | PROXY protocol 標頭不合法：信任的 LB 位址沒有送標頭，或不在 `proxy_protocol_from` 的來源送了標頭；連線已關閉（節流記錄） | 檢查 LB 的 `send-proxy-v2` 與 `proxy_protocol_from` |
| 2301 | | `PLS-W2301` | | 目前的伺服器位址無法連線，已改試下一個候選位址 | 檢查該節點或位址；其他位址可用時服務不中斷 |
| 2302 | | `PLS-E2302` | | 所有候選位址（含 SRV 解析出的位址）都無法連線，依退避重試；每個位址最後的錯誤列在詳細資訊中 | 依各位址的錯誤處理（`PLS-E2002`～`E2004` 等） |
| 2303 | | `PLS-E2303` | | DNS SRV 查詢失敗或沒有紀錄，改用 `server.address` 與 `fallback_addresses` | 檢查 DNS 的 SRV 紀錄或 `server.srv` 的拼寫 |
| 4401 | `KRS-E4401` | | | 無法連線到 PostgreSQL（啟動時：拒絕啟動，結束代碼 3；執行中：report 回 `storage_unavailable`、`/readyz` 503，持續重試） | 檢查網路、`storage.postgres`、密碼檔、TLS |
| 4402 | `KRS-E4402` | | | PostgreSQL 不符合要求：版本低於 14、編碼不是 UTF8、權限不足、TLS 設定不安全、schema 被其他程式使用 | 依訊息修正資料庫或設定 |
| 4403 | `KRS-W4403` | | | PostgreSQL 沒有同步複寫（`synchronous_commit` 不是 on，或沒有同步備庫）：主庫故障時可能遺失已確認的資料 | 設定同步備庫，或接受這個風險 |
| 4404 | `KRS-E4404` | | | 資料搬移（`kairos db migrate`）失敗或驗證不一致；目的端不可使用 | 依訊息修正後以 `--restart` 重新執行 |
| 4405 | `KRS-E4405` | | | 資料庫的 schema 比這個 Kairos 新（叢集中已有較新的版本升級了資料庫）：拒絕啟動 | 把這個節點升級到相同的版本 |
| 8305 | `KRS-W8305` | | | 節點的時鐘與資料庫的時鐘相差超過 2 秒 | 設定 NTP（chrony）；租約不受影響，但時間戳記會偏差 |
| 8401 | `KRS-I8401` | | | 本節點取得 leader 租約（階段 A：開始接受連線）；換手時另記上一個 leader | 資訊 |
| 8402 | `KRS-W8402` | | | 本節點失去 leader 租約（續約失敗或已被其他節點取得），停止單一寫入者的工作（階段 A：結束行程，由 systemd 重新啟動為備援） | 檢查與資料庫的連線；頻繁發生時調高 `lease_ttl_secs` |
| 8403 | `KRS-E8403` | | | 叢集的協調異常：沒有任何節點持有 leader 租約超過 60 秒（告警、彙總、保留、CRL 暫停）；或 `node_id` 與另一個執行中的節點重複（拒絕啟動） | 依訊息：恢復資料庫連線、啟動節點，或修正 `node_id` |
| 8404 | `KRS-E8404` | | | 本節點的金鑰與叢集不一致或缺少（`alerts-secrets.key`、`console.key`、`audit-hmac.key`、簽發 CA 私鑰）：不進入就緒、不參與 leader 選舉、不自動建立金鑰 | 以 `kairos cluster keys import` 匯入叢集的金鑰 |
| 8405 | `KRS-W8405` | | | 叢集節點的設定不一致（需要一致的設定雜湊不同） | 比對 `kairos config show --effective`，統一設定 |
| 8406 | `KRS-W8406` | | | 叢集節點失聯（心跳逾時）；它服務的主機已標為離線（`server_lost`），等待重連到其他節點 | 檢查該節點 |
| 8407 | `KRS-I8407` | `PLS-I8407` | 鏡像 | 節點正在排空：Kairos 要求已連線的 agent 改連其他節點；Pulse 收到後依指定的延遲重連（不算失敗） | 資訊 |

**沿用的既有代碼**：

| 代碼 | 用於 |
|---|---|
| `KRS-E0202` | 設定互相矛盾：叢集卻不是 PostgreSQL、叢集加 JSONL 鏡像 |
| `KRS-E4304` | 資料庫正在使用中：`kairos db migrate` 偵測到 Kairos 仍在執行 |
| `KRS-E4101`／`PLS-W4101` | 寫入失敗 |
| `KRS-E1303` | 簽發 CA 私鑰無法使用 |
| `PLS-E0201` | `fallback_addresses`、`srv` 的值不合法 |

## 20. 實作分工建議

### 20.1 實作線

| 線 | 範圍 | 主要檔案 | 估工 | 相依 |
|---|---|---|---|---|
| **L1a 儲存抽象** | 領域 store 介面、`Tx` 抽象、把既有 SQL 原封不動搬進 `sqlite::*`（純重構，所有測試照過）、CLI 改用領域介面 | `store/`、`alerts/store.rs`、`enroll/store.rs`、`users/store.rs`、`audit.rs`、`pki/store.rs`、`api/db.rs`、各 `*_cli.rs` | 2.5 人週 | 無（**最先開始**） |
| **L1b PostgreSQL 後端** | `postgres::*` 實作；schema v9（兩種後端）；第 5.4 節的並行改寫；稽核鏈（第 12 節）；leader 版彙總與速率（第 7.3、7.4 節）；容量計算；PostgreSQL 的測試與量測（第 7.6 節的目標） | 新 `store/postgres/`、`store/rollup_pg.rs` | 4～4.5 人週（可兩人分：報告路徑與彙總一人，領域 store 一人） | L1a 的介面定稿 |
| **L2 叢集協調（階段 A）** | `[cluster]` 設定；`cluster_nodes` 心跳；租約與 fencing；`LeaderJobs` 監督器；備援的生命週期（不監聽、接手、失去租約時結束）；`cluster_revs` 與 `LISTEN/NOTIFY`；金鑰指紋與停用自動建立；設定雜湊；`/readyz` 與 scope；系統健康 `cluster`；`/metrics`；日誌的 `node`；PROXY protocol v2；告警的換手判斷（第 13.2 節） | 新 `cluster/`、`main.rs`、`probe.rs`、`api/health.rs`、`alerts/mod.rs`、`users/secrets.rs`、`alerts/secrets.rs`、`audit.rs`、`per_ip.rs` | 3 人週 | L1a（介面）；可先以 PostgreSQL 直接開發租約 |
| **L3 遷移與維運** | `kairos db migrate`（兩個方向、驗證、`--reports-since`）；`kairos cluster init／status／keys／drain／step-down／forget`；`kairos.keys-backup/v2`；`--check-config` 的 PostgreSQL 與叢集檢查；`kairos diag` 收集叢集資訊；`packaging/`：設定範例、systemd（`Restart=always`、結束代碼 75）、HAProxy 與 keepalived 範例、INSTALL 的 HA 章節與階段 0 附錄；CHANGELOG 與相容矩陣 | `store/migrate.rs`（新）、`cluster_cli.rs`（新）、`keys_backup.rs`、`config.rs`、`diag.rs`、`packaging/` | 2 人週 | L1b 的 schema、L2 的 CLI 介面 |
| **L4 Pulse 多位址** | `server.fallback_addresses`、候選清單與輪替（第 8.2、8.3 節）、每 IP 的連線時限、註冊端點跟著候選清單（0.10.0，第 8.6 節）、狀態檔與 `pulse status`、`diag`、本機指標；feature `cluster`（`server_node`、`cluster_id`、`drain` 的處理）與 `kairos-proto` 的型別 | `pulse/src/network.rs`（`Dialer`）、`config_manager.rs`、`enroll/`、`status_file.rs`、`status_cmd/`、`diag.rs`、`metrics.rs`、`crates/kairos-proto` | 2 人週 | 無（**可以立即開始**）；Kairos 端的 `drain` 在 L2 或 L5 |
| **L4s Pulse SRV**（選用） | `server.srv`、`hickory-resolver`、RFC 2782 排序與快取 | `pulse/src/network.rs` | 0.5～1 人週 | L4 |
| **L5 多主動（階段 B1）** | `host_presence` 與跨節點踢除（第 6.5 節）；`ClusterView`；`cluster_events`（SSE、匯流排到 leader）；清冊同步改為依節點；告警的主機狀態改讀 `ClusterView`；`/readyz?scope=enroll|console` 只給 leader；排空的 Kairos 端；`decommissioned` 重讀 | `registry.rs`、`connection.rs`、`monitor.rs`、`api/hosts.rs`、`api/events.rs`、`api/overview.rs`、`alerts/mod.rs`、`enroll/mod.rs`、`cluster/` | 3.5～4 人週 | L1b、L2 |
| **L6 HA 端對端與故障注入** | `tests/e2e` 的叢集情境：PostgreSQL＋2～3 個 Kairos＋HAProxy 的容器組合（在 kp-build 執行）；第 17 節 F1～F17 的自動化（殺行程、凍結、`iptables` 隔離資料庫、PostgreSQL 切換）；驗證「補送不缺號、不重複」、稽核鏈連續、告警不重複開事件、通知最多重送一則 | `tests/e2e/` | 1.5 人週 | L1b、L2（階段 A）；L5（階段 B） |
| （B2，之後） | 主控台與 8444 多主動：`login_challenges`、`enroll_nonces`、`auth_failures`、L7 重讀、非 leader 的告警動作轉送 | — | 1.5 人週 | L5 |

### 20.2 建議的排程

```text
週 1–3    L1a（1 人）           L4（1 人）
週 3–7    L1b（2 人）           L2（1 人，從週 3 起以 PostgreSQL 直接開發）   L4s（選用）
週 7–9    L3（1 人）            L6 階段 A（1 人）
          ── 階段 A 可發佈（0.9 或 1.0 的 HA 預覽）──
週 9–13   L5（1～2 人）         L6 階段 B
```

### 20.3 估工與審計的差異

- **階段 A**：L1a＋L1b＋L2＋L3＋L6 ≈ 13～13.5 人週。若只做 PostgreSQL 的領域 store 而延後彙總改寫（階段 A 只有一個 writer，可以先沿用記憶體桶），可減到約 11～12 人週。
- **階段 B**：L5＋L6 的增量 ≈ 4～5 人週。
- **Pulse**：L4（＋L4s）≈ 2～3 人週。

合計約 17～21 人週（階段 B 不含 B2），高於審計的 6～10 人週。主因：

1. **沒有資料庫抽象層**：381 個 SQL 呼叫點直接使用 rusqlite，CLI 也直接開資料庫，必須先重構（L1a）；
2. **31 處依賴 SQLite 全域寫入鎖的交易**需要逐一改寫並發語意並加測試（D1～D9）；
3. **單一寫入者的工作**遍布告警、註冊、PKI、稽核與儲存五個子系統；
4. 審計估算時（0.1）還沒有告警、PKI、使用者與稽核鏈。

## 21. 已決定（2026-10-19，採建議）

使用者於 2026-10-19 決定全部採用「建議」欄的選項（各項結論見最右欄）。規格內容沒有因此修改。

| # | 項目 | 選項 | 建議 | 結論（2026-10-19） |
|---|---|---|---|---|
| 1 | 叢集使用的資料庫 | (a) PostgreSQL；(b) 只提供 SQLite＋作業層冷備援（階段 0），不做產品內的 HA；(c) SQLite＋LiteFS | **(a)**。(b) 無法達成「已 `ack` 不遺失＋自動切換」，(c) 依賴 Consul 與維護活躍度下降的專案 | 採建議：(a) |
| 2 | 階段 A 的備援節點是否監聽 | (a) 不監聽，`/readyz` 503；(b) 監聽但拒絕連線；(c) 唯讀主控台 | **(a)**。最簡單，不會有兩個節點同時接受 agent 連線 | 採建議：(a) |
| 3 | PostgreSQL 的最低版本 | 13／14／15 | **14**（目前主流 LTS 發行版都能取得；13 已於 2025-11 結束社群支援） | 採建議：14 |
| 4 | PostgreSQL 驅動與連線池 | (a) `postgres`＋`tokio-postgres-rustls`＋自行實作的小型池；(b) 同 (a)＋`r2d2-postgres`；(c) `sqlx` | **(a)**，依賴最少，保留同步的 store 結構 | 採建議：(a) |
| 5 | 已 `ack` 的資料的 RPO | (a) 沒有同步複寫時只警告（`KRS-W4403`）；(b) 拒絕啟動（可以設定略過） | **(a)**。與 SQLite `synchronous = normal` 的取捨一致，由客戶決定 | 採建議：(a) |
| 6 | 簽發 CA 私鑰的分發 | (a) 每個節點放同一組；(b) 只有 leader 持有，其他節點轉送簽發；(c) 以 KEK 加密存在資料庫 | **(a)**，(c) 列為之後的強化 | 採建議：(a) |
| 7 | 對稱金鑰的分發 | (a) `kairos cluster keys export/import`（加密檔）；(b) 存在資料庫、以外部 KEK 加密 | **(a)**，沿用 0.7 的加密備份格式 | 採建議：(a) |
| 8 | Pulse 多位址的設定形式 | (a) 新鍵 `server.fallback_addresses`；(b) `server.address` 接受陣列 | **(a)**：降版安全，`pulse enroll` 的改寫工具不需要支援陣列 | 採建議：(a) |
| 9 | DNS SRV | (a) 第一版就做；(b) 先做 `fallback_addresses`，SRV 列為階段 B 的選用；(c) 不做 | **(b)**：SRV 需要新的 DNS 依賴，地端客戶多半沒有可自行管理 SRV 的 DNS | 採建議：(b) |
| 10 | 安裝字串是否支援多位址 | (a) 不改（`pulse-enroll:v1:`，用服務名稱）；(b) 新增 `pulse-enroll:v2:` 帶多個 `s` | **(a)** | 採建議：(a) |
| 11 | 節點失聯時主機的離線原因 | (a) 新增 `server_lost`；(b) 沿用 `idle_timeout` | **(a)**。主控台、Webhook、`/metrics` 的 `offline_reason` 多一個值，接收端應忽略不認得的值（設計包待辦） | 採建議：(a) |
| 12 | SSE 在階段 A 換手後 | (a) `resync`，原因新增 `failover`；(b) 沿用 `restarted` | **(a)**，主控台的處理相同，只是文案不同 | 採建議：(a) |
| 13 | 階段 B 的主控台與 8444 | (a) B1：只由 leader 服務；(b) 直接做 B2（多主動） | **(a)**，B2 視實際需求再做 | 採建議：(a) |
| 14 | HA 是否需要授權閘 | (a) 等授權機制完成後再加；(b) 現在就以設定旗標隱藏 | **(a)**（商業審計：HA 是收費功能，但授權尚未實作） | 採建議：(a) |
| 15 | PostgreSQL 後端的彙總 | (a) leader 從原始資料產生；(b) 各節點的記憶體桶加上可交換的合併 | **(a)**。(b) 在節點當掉時無法判斷哪些值已計入 | 採建議：(a) |
| 16 | `storage.max_total_bytes` 在 PostgreSQL 上 | (a) 以 Kairos 資料表的大小計算，行為與 SQLite 相同；(b) 停用，交給 DBA | **(a)**，預設值不變 | 採建議：(a) |
| 17 | 叢集是否支援 JSONL 後端與鏡像 | 支援／不支援 | **不支援** | 採建議：**不支援** |
| 18 | schema 改變時的升級 | (a) 全部停止後升級（停機約數分鐘）；(b) 之後設計 expand／contract 的線上遷移 | **(a)**，(b) 另立規格 | 採建議：(a) |
| 19 | 失去租約的節點 | (a) 結束行程，由 systemd 重新啟動（階段 A）；(b) 留在行程內降級 | 階段 A **(a)**；階段 B 降為 member（連線不受影響） | 採建議：階段 A **(a)**；階段 B 降為 member |
| 20 | 版本號 | (a) 階段 A 以 0.9.0 發佈（標示 HA 預覽）；(b) 等階段 B 完成後以 1.0 發佈 | **(a)**，讓單機客戶先取得 schema v9 與 PostgreSQL 後端 | 採建議 **(a)** 的方向（以 0.9.x 線逐步發佈、不等 1.0）；依同日的範圍決定，0.9.0 只含 L1a 與 L4，PostgreSQL 後端（schema v9）與叢集協調（階段 A，標示 HA 預覽）在 **0.10.0** |

## 22. 設計包待辦（本線沒有修改 `design/`）

1. **`copywriting.md` §3.2、§3.3**：
   - 新增子類別 23、44、84；
   - 正式分配第 19 節的 18 個代碼（Kairos 14、Pulse 4，其中 `8407` 為鏡像）；
   - 補 §4.2 的三段式範例（至少 `KRS-E4401`、`KRS-W4403`、`KRS-E8404`、`KRS-W8406`、`PLS-W2301`、`PLS-E2302`）；
   - `prototype/kairos/error-codes.js` 以 `tools/sync-error-codes.js` 重新產生。
2. **`kairos/screens.md` S20（系統健康）**：新增 `cluster` 項目的呈現（節點表、leader、資料庫的同步複寫、時差），以及「回應的節點」標示。
3. **新畫面或 S21 的分頁「叢集」**：
   - 節點清單（角色、版本、心跳、連線數、設定一致性、金鑰狀態）；
   - 排空與 step-down 的確認（L2 或 L3 等級）；
   - 權限 `cluster:write`（`ia.md` §4.2 的權限表）。
4. **離線原因 `server_lost`**：主機詳情、連線紀錄、Webhook 的 `offline_reason` 文案（`alerts-ux.md` §6.6）。
5. **告警**：
   - 新的系統規則（第 13.4 節）的名稱、說明與建議處理；
   - 「叢集的 leader 已切換」「叢集沒有 leader 達 N 分鐘」取代換手情境下的「Kairos 已重新啟動」「停機 N 分鐘」；
   - 系統事件的對象顯示節點名稱。
6. **Webhook `kairos.alert.v1` 新增 `delivery_id`**、Email 新增 `X-Kairos-Delivery-Id`：§6.6 的範例。
7. **SSE `resync` 新增原因 `failover`**：主控台的提示文案（「伺服器已切換，正在重新載入」）。
8. **Pulse**：
   - `pulse status` 的「已連線」列顯示節點與候選位址；
   - 所有位址都失敗時的詳細資訊排版（`pulse/` 的 status 規格）；
   - `PLS-I8407` 的日誌文案。
9. **`naming.md`**：`node_id`、`cluster_id` 的命名規則與顯示方式；「leader」「備援」「排空」的台灣用語（建議：leader＝「主節點」、standby＝「備援節點」、drain＝「排空」）。

## 23. 測試計畫（摘要）

- **單元與元件**：
  - 領域 store 的參數化測試（SQLite 與 PostgreSQL）；
  - 租約的取得、續約、到期與 fencing（以可控的資料庫時間）；
  - D2～D8 的並行測試（兩條連線同時操作）；
  - 稽核鏈在 8 條連線同時寫入時仍是單一條鏈（驗證通過、`id` 連續）；
  - 彙總的 SQLite 與 PostgreSQL 演算法以同一組向量比對；
  - Pulse 的候選清單排序、輪替與 `drain` 的處理（以 `Dialer` 的假實作）。
- **端對端**（`tests/e2e`，kp-build）：
  - 第 17 節的每個情境至少一個測試；
  - 「1,000 台模擬 agent、durable、F1 換手 10 次」後 `reports` 不缺號、不重複；
  - 告警事件不重複開；
  - 稽核鏈驗證通過。
- **量測**：
  - PostgreSQL 後端在 kp-build 上重跑 STORAGE.md 第 11 節的 `sqlite_throughput` 與補送尖峰（改名為 `store_throughput`，以環境變數選擇後端）；
  - 換手時間的分布（F1、F2 各 20 次）。
