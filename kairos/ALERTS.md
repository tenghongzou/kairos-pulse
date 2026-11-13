# Kairos 告警引擎（0.6.0）

商業審計 F3；設計包 `kairos/alerts-ux.md`（全文）、`kairos/screens.md` S06～S11 與 §24 #2、#8、#9，決策 D3（四級嚴重度）、D23（靜音範圍）、D26（靜音時長上限）。端點見 [`API.md`](API.md) 第 13 節，資料表見 [`STORAGE.md`](STORAGE.md) 第 3 節（schema v5）。

- 實作：`kairos/src/bus.rs`（事件匯流排）、`kairos/src/alerts/`（引擎、系統檢查、通知、資料表）、`kairos/src/api/alerts.rs`（端點）、`kairos/src/alerts_cli.rs`（CLI）。
- 需要 SQLite（`storage.backend = "sqlite"`，預設）；`[alerts] enabled = false` 時不啟動，告警端點回 503 `alerts_unavailable`。

## 1. 架構

```
各元件（狀態表、連線、TLS 重載、註冊通道、除役）
        │ 發布結構化事件（bus::EventKind）
        ▼
   事件匯流排（bus::EventBus，broadcast）──→ 之後的 /metrics、稽核外送也可以訂閱
        │
        ▼
告警服務（自己的執行緒，current-thread runtime）
  每 1 秒：匯流排的事件、CLI 的動作佇列（每 2 秒）
  每 5 秒：一個判定週期 ── 狀態表＋清冊（30 秒讀一次）→ HostView
                        ── 系統檢查（probe）→ SystemCondition
                        ── 引擎（engine＋eval，同步、不做 I/O）→ 事件、處理紀錄、通知、SSE
                        ── 一個交易寫入資料庫 → 派送（notify）
```

- **不拖慢 report 的 `ack`**：report 的寫入路徑沒有新增任何工作（寫入失敗只累加既有的 `WriteStats` 計數）；引擎讀狀態表只複製參考計數，在自己的執行緒上判定，寫入資料庫以短交易進行（與註冊服務同一個 `EnrollDb`，兩次寫入至少相隔 2 ms，讓 report 的 writer 排得到寫入鎖）。量測見第 9 節。
- **事件只是提示**：匯流排滿了（訂閱者落後 8,192 則）會遺失事件（`status.bus_dropped` 計數）；主機的上線、離線一律以每個週期讀取的狀態表為準（準位觸發），匯流排只用於被取代、身分驗證失敗、除役、TLS 重載、狀態表淘汰等沒有其他來源的訊號。

## 2. 事件匯流排（§24 #2、#8 的事件部分）

| `type` | 發布者 | 內容 |
|---|---|---|
| `host_online` | 狀態表（上線、重新上線、離線後恢復） | `agent_id`、`hostname`、`ip` |
| `host_offline` | 狀態表 | 另有 `reason`（下表） |
| `host_superseded` | 狀態表（同一個 agent_id 的新連線取代仍在的舊連線） | `previous_ip`、`ip`（身分衝突 `KRS-W3102` 的訊號） |
| `auth_failed` | 連線（hello 的 agent_id 與用戶端憑證不符，`KRS-W1102`） | `agent_id`（自稱）、`ip` |
| `registry_evicted` | 連線（狀態表已滿，淘汰離線最久的主機，`KRS-E3201`） | `agent_id`、`hostname` |
| `host_decommissioned` | 8443 的 `deregister` 成功 | `ip`、`initiated_by`、`already` |
| `host_rebooted` | 狀態表（0.8；report 的 `system.uptime` 比上一筆小、且取樣時間較晚） | `agent_id`、`hostname`、`uptime_secs`、`previous_uptime_secs`、`boot_time`（推算） |
| `tls_reload` | TLS 重載工作 | `ok`、`crl`（變更涉及 CRL）、`message` |
| `condition` | 系統檢查（狀態**變化**時才發布：成立、嚴重度改變、解除） | `check`（規則 ID）、`subject`、`active`、`severity`、`code`、`summary` |
| `lifecycle` | 主程式 | `phase`：`started`、`shutting_down` |
| `channel_secrets` | 告警服務（0.7；管道的機密變成無法解開或恢復時） | `channel_id`、`name`、`available` |
| `audit_integrity` | 稽核的定期自我驗證（0.7；每次驗證） | `ok`、`reason`、`id`（斷點）、`summary` |
| `upgrade_window` | 遠端管理服務（0.10；推送中的主機進入 `applying`、到達終止狀態或期限已過時，第 5.5 節） | `agent_id`、`rollout_id`、`until_ms`（抑制的期限；解除時 `null`） |

**斷線原因分類**（Webhook 的 `offline_reason` 固定代碼，`alerts-ux.md` §6.6）：`idle_timeout`（逾時，未收到心跳）、`agent_closed`（agent 關閉或網路中斷）、`certificate_rejected`（連線中重新驗證時憑證被拒）、`server_shutdown`（Kairos 關機，永遠不產生告警）、`write_unknown`（report 寫入結果未知，Kairos 關閉了連線；0.6 新增）、`server_lost`（服務這台主機的叢集節點失聯；0.9 定義名稱，叢集模式才會產生，`kairos/HA.md` 第 6.6 節；依一般的主機離線寬限判斷，Pulse 改連其他節點後重新上線）。接收端應忽略不認得的值。**0.7 起主機 API、SSE 與 `/metrics` 都使用這組名稱**（過渡期主機 API 另帶舊名稱的 `offline_reason_legacy`，`API.md` 第 5.2 節）。`auth_failed`、`superseded` 不是離線原因（前者在 hello 之前被拒、後者主機仍在線上），以各自的事件發布；`revocation_unavailable` 保留給之後區分「CRL 問題造成的拒絕」（目前歸在 `certificate_rejected`，握手時被拒的另由 `tls::rejections` 記錄原因 `revocation_status_unavailable`）。

`storage_unavailable`、寫入結果未知、retention 暫停、連線上限、CRL／伺服器憑證／簽發 CA 即將到期、註冊端點的過載／封鎖／限流／時鐘偏差／自動暫停，由系統檢查讀取各元件的計數與狀態，以 `condition` 發布（第 5.3 節）。`/healthz`、`/metrics`、JSON 日誌屬於另一條線（`sprint5/obs`），可以直接訂閱匯流排。

## 3. 設定（`[alerts]`）

```toml
[alerts]
enabled = true            # false：不啟動告警引擎
console_url = ""          # 通知中的事件連結（例 https://monitor.example.com）；空字串時不附連結
timezone = "+08:00"       # 通知的時間與事件編號的日期：固定位移（不支援時區名稱）
warmup_secs = 600         # 啟動後的告警暖機期，60～3600
retention_days = 400      # 已解決事件的保留天數（含處理紀錄、發送紀錄），1～3660
max_events = 100000       # 事件筆數上限（超過時刪除最舊的已解決事件），1,000～10,000,000
subnet_prefix_v4 = 24     # 網段彙總的遮罩
subnet_prefix_v6 = 64
```

只改 `[alerts]` 時需要重新啟動。規則、靜音、通知管道存在資料庫，由 API／CLI 修改，執行中的 Kairos 在 5 秒內重新載入（`meta.alerts_config_rev`）。

## 4. 資料模型

| 項目 | 值 |
|---|---|
| 嚴重度（D3） | `emergency` 緊急、`critical` 嚴重、`warning` 警告、`info` 資訊 |
| 處理類型（§2.3） | `action` 需要處理、`advisory` 規劃提醒、`record` 紀錄（資訊等級一律為紀錄） |
| 狀態（§5.1） | `firing` 觸發中、`acknowledged` 已確認、`recovered` 已恢復（`acked` 區分是否已確認）、`resolved` 已解決 |
| 類別 | `host`、`identity`、`aggregate`（網段、大規模、重新啟動後未恢復）、`system` |
| 根因 | 使用者：`hardware`、`network`、`unregistered_maintenance`、`software`、`false_positive`、`other`；系統：`self_recovered`、`condition_cleared`、`auto_closed`、`decommissioned` |
| 事件編號 | `E-YYYYMMDD-NNNN`（`alerts.timezone` 的日期，每天從 0001 起） |
| 去重鍵 | `規則 ID|對象`（主機為 agent_id、網段為 CIDR、系統事件為檢查項目） |

## 5. 引擎

### 5.1 判定（準位觸發）

每個週期算出每條規則「目前成立的對象」：成立且沒有事件 → 開新事件；事件已恢復 10 分鐘內再次成立 → 重新開啟同一事件（`reopen_count`）；對象不再成立 → 已恢復（規劃提醒與紀錄直接為已解決，根因 `condition_cleared`）。重新啟動後載回未解決的事件，同一套比對自然延續。

- **異常的開始時間**（`episode`，主機離線為離線的時間）：手動「解決」時條件仍成立，同一段異常不再開新事件；主機恢復後再次離線才是新的一段。
- **抖動**：同一對象 30 分鐘內狀態轉換 4 次以上 → `flapping`，之後的重新開啟與恢復不逐次通知，改為每 30 分鐘一則摘要；穩定 30 分鐘後解除。
- **生命週期**：已恢復、未確認 24 小時後自動結案（根因 `self_recovered`），**緊急除外**（留在「需要我處理」）；已恢復、已確認 7 天後自動結案（`auto_closed`）。
- **使用者的動作**：確認（停止重送與升級、可「確認並指派給我」與預計處理時間；ETA 到期記一筆處理紀錄）、指派、暫停通知（1～24 小時）、解決（必選根因，有未恢復的子事件時它們脫離成為獨立事件）、註解（已解決的事件也可以）。規劃提醒與紀錄回 409 `not_acknowledgeable`。

### 5.2 關聯彙總與暖機期（§3.3）

| 層 | 事件 | 吸收 |
|---|---|---|
| 系統 | 撤銷清單已過期（`sys.crl_expired`） | 憑證被拒（`revocation_status_unavailable`）與因憑證被拒而離線的主機事件 |
| 系統 | 同一 IP 連線數超過上限（`sys.per_ip_limit`，每個來源一則） | 同一來源、逾時或 agent 關閉而離線的主機 |
| 系統 | 連線數已達上限（`sys.connections`，暫停接受新連線時） | 逾時或 agent 關閉而離線的主機 |
| 彙總 | 大規模離線（`sys.mass_offline`）：120 秒內 ≥ 50% 的線上主機（至少 10 台）逾時或 agent 關閉 | 時間窗內成立的網段事件與其他離線主機 |
| 彙總 | 網段離線（`sys.subnet_offline`）：120 秒內同一網段 ≥ 5 台、且 ≥ 30% 的線上主機 | 成立後 30 分鐘內該網段離線的主機 |
| 彙總 | 重新啟動後未恢復連線（`sys.restart_unrecovered`） | 暖機期結束時仍未連上的主機 |

- 子事件照常記錄、在父事件頁列出，**不通知**、不計入徽章；父事件以子事件 90% 恢復判定恢復，仍未恢復的子事件脫離成為獨立事件並以**一則**彙整通知（同一組合併）。父事件在涵蓋的台數跨越 10、50、100、500 時各送一則更新。
- **先彙總再通知**：網段事件成立後先等 60 秒才通知，期間成為大規模離線的子事件就不另外通知；大規模離線先於網段判定，同一週期成立的網段事件直接成為它的子事件。主機離線的寬限（預設 3 分鐘）長於時間窗（120 秒），任何單機通知之前已決定是否併入彙總。
- **暖機期**：啟動後到「重新啟動前在線上的主機（清冊的 `last_seen` 在上次心跳前 2 分鐘內）有 98% 重新連上」或 `alerts.warmup_secs`（預設 10 分鐘）為止，不開新的主機離線事件；結束時仍未連上的主機合併成一則「Kairos 重新啟動後有 N 台主機未恢復連線」。**Kairos 啟動前就已離線的主機不開主機離線事件**（升級與重新啟動不灌出通知；離線超過 24 小時另有「主機長期失聯」）。關機原因的離線永遠不產生事件。
- **停機與重新啟動**：引擎每 30 秒寫一次心跳（`meta.alerts_heartbeat`），正常關機時寫 `clean: true`。啟動時記一則「Kairos 已重新啟動」（資訊；上次沒有正常關機時為嚴重），停機超過 5 分鐘且不在維護時段內時另開「Kairos 停機 N 分鐘，期間資料未收集」（嚴重，直接為已恢復、需要確認）。

### 5.3 系統預設規則（§2.2）

不可刪除，停用必須填原因；可調整通知管道與通知設定，條件（`params` 中既有的數值）只有 `admin` 範圍能改，系統類規則的嚴重度不可低於預設。依剩餘天數升級的規則（CRL、伺服器憑證、連線數）是**同一事件逐級升級**，升級時通知一次。

**受保護的系統規則**（由程式碼決定，`src/alerts/rules.rs` 的 `UNSILENCEABLE`、`ALWAYS_ENABLED`；API 的 `unsilenceable`、`always_enabled`）：

| 規則 | 不可靜音 | 不可停用 |
|---|---|---|
| `sys.unauthenticated`（未認證連線已允許） | 是 | 否 |
| `sys.rollout_approval_disabled`（雙人覆核已關閉；0.10.1 起） | 是 | 是 |

- **不可靜音**：事件不受任何靜音影響——包含明確選取它的臨時靜音，以及升級前就已經建立的靜音。建立或修改靜音時在 `rules` 選取它回 400 `rule_unsilenceable`（`details.rule_id`）；事件詳情的 `actions` 沒有 `silence`；主控台的靜音表單不能勾選。
- **不可停用**：`POST /api/v1/alert-rules/{rule_id}/disable` 回 409 `rule_always_enabled`，並寫一筆稽核（`alert_rule.disable`、結果 `rejected`）；主控台的規則清單顯示「啟用（不可停用）」而不是開關。
- **既有的資料庫列**：系統預設規則只在不存在時寫入，升級上來的環境裡這些列是舊版的內容（`params` 沒有 `unsilenceable`，也可能已經被停用）。所以保護不依賴那一列：(1) 啟動時 `ensure_builtin_rules` 把受保護的列同步成程式碼規定的樣子（重新啟用、清掉停用原因、補上 `params.unsilenceable`；不需要資料庫遷移）；(2) 引擎載入規則時（啟動與每次重新載入）再套一次，資料庫在執行期間被直接改掉也照常判定、照常通知。
- 仍然可以調整的：通知管道與通知設定（沒有接管道就不會推播，事件照常記錄）、把嚴重度調高。事件本身可以確認、暫停重送、解決，這些動作各自留在稽核紀錄。

| 規則 ID | 名稱 | 預設條件 | 等級 | 代碼 | 資料來源 |
|---|---|---|---|---|---|
| `sys.host_offline` | 主機離線 | 離線持續 ≥ `grace_secs`（180） | 嚴重 | — | 狀態表 |
| `sys.host_long_offline` | 主機長期失聯 | 離線 ≥ `after_secs`（86,400） | 警告 | — | 狀態表＋清冊 |
| `sys.report_stale` | 回報延遲 | 線上但最近一次 report 超過 `factor`（3）× 宣告的 report 間隔；回報無法寫入期間抑制 | 警告 | — | 狀態表 |
| `sys.identity_conflict` | 身分衝突 | `window_secs`（3600）內被取代 ≥ `min_count`（5）次、來源 ≥ `min_sources`（2）個 | 嚴重 | `KRS-W3102` | 匯流排 |
| `sys.cert_rejected` | 憑證被拒（單機） | 握手時被拒的憑證歸屬到主機（已撤銷 `KRS-W1105`、過期 `KRS-W1108`），或 `auth_failed`（`KRS-W1102`）；管理員撤銷（PKI 中已撤銷的序號）、已除役的主機、主機之後已上線者不觸發 | 嚴重 | 同左 | `tls::rejections`、匯流排 |
| `sys.host_decommissioned` | 主機自行除役 | 每台一則紀錄；之前觸發的主機事件自動結束 | 資訊 | — | 匯流排 |
| `sys.host_rebooted` | 主機重新開機（0.8，D5） | `system.uptime` 變小（需要 Pulse 0.8.0）；標題「{主機} 已重新開機」，內容有前後的運作時間與推算的開機時間；建立後立即結案，只留紀錄、**不通知**，不需要確認 | 資訊（紀錄） | — | 匯流排（`host_rebooted`） |
| `sys.mass_decommission` | 短時間大量除役 | 10 分鐘內 ≥ 20 台；60 分鐘沒有新的除役時恢復 | 警告 | `KRS-W3108` | 匯流排 |
| `sys.subnet_offline`、`sys.mass_offline`、`sys.restart_unrecovered` | 第 5.2 節 | | 緊急、緊急、嚴重 | — | 狀態表 |
| `sys.crl_expiring` | CRL 即將到期 | 剩 7／3／1 天 | 警告→嚴重→緊急 | `KRS-W1201` | TLS 設定的到期日 |
| `sys.crl_expired` | CRL 已過期 | 超過 `nextUpdate` | 緊急 | `KRS-E1203` | 同上 |
| `sys.server_cert_expiring` | 伺服器憑證即將到期 | 剩 30／14／7 天 | 警告→嚴重→緊急 | `KRS-W1001` | 同上 |
| `sys.report_writes` | 回報無法寫入 | 過去 5 分鐘有 `storage_unavailable` 或寫入結果未知 | 緊急 | `KRS-E4101` | `WriteStats` |
| `sys.db_capacity` | 資料庫將達／已達總容量上限 | 預估超過 `storage.max_total_bytes`，或 24 小時內曾提早刪除 | 警告 | `KRS-W4202`（已達） | 資料庫 `meta`、狀態表 |
| `sys.retention_paused` | 保留清除暫停 | 日期保護暫停中 | 警告 | `KRS-W4201` | 資料庫 `meta` |
| `sys.registry_capacity` | 狀態表接近上限／已淘汰 | ≥ 80% `limits.max_agents`，或過去 1 小時淘汰過 | 警告 | `KRS-E3201` | 狀態表、匯流排 |
| `sys.connections` | 連線數接近上限 | ≥ 90% 或過去 1 小時因全域上限被拒為警告；≥ 98% 或暫停接受新連線為嚴重 | 警告→嚴重 | `KRS-W2204` | 狀態表、`per_ip` 計數 |
| `sys.per_ip_limit` | 同一 IP 連線數超過上限 | 過去 1 小時有連線因每 IP 上限被拒（每個來源一則） | 警告 | `KRS-W2201` | `per_ip` 計數 |
| `sys.enroll_overload` | 註冊端點過載或佇列已滿 | 過去 10 分鐘有 503（`enroll_busy`／`pending_queue_full`） | 嚴重 | `KRS-W2205` | 註冊服務計數 |
| `sys.enroll_blocked` | 註冊來源被暫時封鎖 | 過去 1 小時有封鎖 | 警告 | `KRS-W3005` | 同上 |
| `sys.enroll_rate_limited` | 註冊端點速率限制 | 過去 1 小時有 429 | 資訊（紀錄） | `KRS-W2202` | 同上 |
| `sys.enroll_clock_skew` | 註冊因時鐘偏差暫不受理 | 最近 15 分鐘被拒的主機（彙總一則，台數歸 0 時結束） | 資訊（紀錄） | `KRS-W8302` | 同上 |
| `sys.enroll_auto_paused` | 註冊端點自動暫停 | 自動暫停中 | 資訊（紀錄） | — | 同上 |
| `sys.enroll_closed_default` | 註冊端點維持關閉（升級） | 設定檔沒有 `[enroll]` 的 mode；啟動時一則 | 資訊（紀錄） | `KRS-I2010` | 設定 |
| `sys.tls_reload_failed` | TLS 設定重新載入失敗 | 熱重載失敗，直到下一次成功 | 嚴重 | 涉及 CRL 時 `KRS-E1204` | 匯流排 |
| `sys.unauthenticated` | 未認證連線已允許 | 未啟用 mTLS 卻監聽對外位址；每日提醒一次、**不可靜音** | 嚴重 | `KRS-W0302` | TLS 設定 |
| `sys.channel_failed` | 通知管道失敗 | 同一管道連續 3 次發送失敗（每個管道一則）；改送其他所有健康的管道。0.7：機密無法解開（`secrets_unavailable`）時不必等到發送失敗，立即成立 | 嚴重 | `KRS-E5107`；機密無法解開 `KRS-E5110` | 派送 |
| `sys.audit_integrity` | 稽核日誌完整性（0.7） | 定期自我驗證（`audit.verify_interval_hours`，預設每 24 小時）發現雜湊鏈或 HMAC 斷裂、HMAC 金鑰遺失或不符；下一次驗證通過時恢復 | 緊急 | `KRS-E6201` | 匯流排（`audit_integrity`） |
| `sys.rollout_paused` | 升級推送已暫停（0.9） | 升級推送因失敗比例超過上限、主機回滾失敗或發佈簽章驗證失敗而自動暫停（手動暫停不開）；推送繼續或取消時恢復（REMOTE.md 第 4.7.4 節） | 警告（回滾失敗、簽章失敗為嚴重） | `KRS-W8414` | 匯流排（`remote_condition`，遠端管理服務每 5 秒調和） |
| `sys.upgrade_rollback_failed` | 升級回滾失敗（0.9） | 主機升級後回滾也失敗，需要人工處理；對象為主機；標記為已處理或重試時恢復 | 嚴重 | `KRS-E8408` | 匯流排（`remote_condition`） |
| `sys.release_signature_failed` | 發佈簽章驗證失敗（0.9） | 主機驗證發佈包的簽章失敗（`PLS-E8401`），使用同一版本的推送已暫停；對象為目標版本；推送繼續或取消時恢復 | 嚴重 | `KRS-E8401` | 匯流排（`remote_condition`） |
| `sys.rollout_approval_disabled` | 雙人覆核已關閉（0.10） | Kairos 啟動時發現 `[remote] rollout_approval` 由 `required` 改為 `none`（與資料庫記錄的上一次設定不同；REMOTE.md 第 5.4 節、ADR-0002 決策點 2i）：送出的推送不再需要另一位管理員核准。對象為 `remote`；改回 `required` 並重新啟動後恢復，**不會自己結束**。0.10.1 起**不可靜音、不可停用**（上述「受保護的系統規則」）。預設推播——啟用雙人覆核的單位請把這條規則接到通知管道 | 警告 | —（尚未分配） | 匯流排（`remote_condition`） |
| `sys.restarted`、`sys.downtime` | 第 5.2 節 | | 資訊（非預期：嚴重）、嚴重 | — | 心跳 |

**使用者規則**（S09，`POST /api/v1/alert-rules`）：`host_offline`、`host_long_offline`、`report_stale`、`metric_threshold`（`>`、`>=`、`<`、`<=`，持續 `for_secs`，可設恢復門檻與恢復持續時間——恢復門檻不可在觸發門檻的另一側，`KRS-E5001`）、`identity_conflict`。範圍為標籤、網段、主機（同欄位「或」、跨欄位「且」）。

**指標門檻的 `metric`**（0.8 起擴充；[`METRICS.md`](../crates/kairos-proto/METRICS.md) 第 11.1 節）：

| `metric` | 值 | 門檻範圍 | 對象（事件的 `subject`） |
|---|---|---|---|
| `cpu`、`memory` | 使用率 % | 0～100 | 每台主機（`<agent_id>`） |
| `disk` | 每個掛載點的使用率 %（Pulse 0.2.0～0.7.0 為推算值，事件 `details.estimated: true`） | 0～100 | **每個掛載點各自觸發與恢復**（`<agent_id>\|<掛載點>`）；`params.mountpoints`（選用，glob 清單，例 `["/", "/data*", "C:\\"]`）限定範圍 |
| `swap` | `swap.used ÷ swap.total` % | 0～100 | 每台主機；總量為 0 時沒有值（不觸發） |
| `load` | 5 分鐘平均負載 ÷ 邏輯處理器數（倍；處理器數取自資產） | 0～100 | 每台主機；Windows（沒有負載）與沒有資產的主機沒有值 |
| `network_rx`、`network_tx` | 每個介面的接收／傳送速率 Mbps（位元組速率 × 8 ÷ 10⁶） | 0～1,000,000 | 每個介面（`<agent_id>\|<介面>`）；`params.devices`（選用，glob）限定範圍 |

- glob 只支援 `*`、`?`，區分大小寫，每個清單最多 16 個（設計包 §30.5 #11）；`mountpoints` 只用於 `disk`、`devices` 只用於網路，其他組合 `KRS-E5001`。
- 評估方式不變：每 5 秒以狀態表的最近一次 report 判斷（`LastReport` 另保存每個掛載點的使用率、置換空間、負載、介面與磁碟的速率；速率以最後兩筆 report 算出）。離線主機沒有值。
- 預覽與回測（`POST /api/v1/alert-rules/preview`）以 `rollup_5m` 的平均值重播：磁碟用 `system.filesystem.usage`（沒有時用推算值），網路用速率序列，負載用資產的處理器數。回應另帶 `hosts_without_metric`（範圍內沒有這個指標資料的台數，「符合主機數」照算）、`not_reported`（原因的台數：`old_pulse`（Pulse 0.8.0 以前，置換空間與負載）、`windows`（負載）、`no_inventory`（負載，沒有資產的處理器數）、`no_data`）、`swap_total_zero`（置換空間總量為 0）、`items_in_scope`（磁碟與網路：涉及的掛載點／介面數）（設計包 §30.5 #5）。
- 範本（`alerts-ux.md` §2.2，主控台 S09）：「磁碟使用率 > 90% 持續 10 分鐘，低於 85% 恢復」（警告）、「磁碟使用率 > 95% 持續 5 分鐘」（嚴重）。

### 5.4 靜音與維護時段（§4；D23、D26）

| | 臨時靜音（`silence`） | 維護時段（`maintenance`） |
|---|---|---|
| 時長 | **伺服器端驗證**：一般 7 天；選了系統規則 24 小時；包含緊急等級的規則（含依天數升級到緊急者）或從緊急事件建立 4 小時；同時符合取最短（不選規則＝所有規則，含緊急等級 → 4 小時） | 一次性，最長 366 天 |
| 系統事件 | 只有在「規則」明確選取時才靜音 | 不抑制系統事件 |
| 範圍 | 標籤、網段、主機（同欄位「或」、跨欄位「且」；全部不填＝全部主機）；`groups` 回 400（清冊還沒有群組模型） | 同左 |

- 靜音只停止通知：事件照常記錄、標示 `silenced`（維護時段另標 `in_maintenance`），不計入徽章、不重送、不升級。
- 彙總事件只在範圍不限主機或範圍的網段涵蓋該網段時被靜音（保守：不因部分主機在範圍內就靜音整個彙總）。
- 結束時（到期或提前結束）仍有範圍內未恢復的事件 → 立即送出一則「已結束，仍有 N 個事件未恢復」。
- 範圍超過 1,000 台主機、所有規則＋全部主機、或包含緊急等級時，`preview` 回 `confirm.required`（主控台以 L1 確認；CLI 互動時詢問、非互動需 `--yes`）。

### 5.5 升級中主機的離線告警抑制（0.10；REMOTE.md 第 4.7.5 節）

升級推送中的主機在替換、重新啟動與健康確認期間（`rollout_hosts` 的 `applying` 到終止狀態之間）會短暫離線。遠端管理服務以匯流排的 `upgrade_window` 告訴引擎每台主機的抑制期限（`applying_at + health_timeout_secs + 10 分鐘`；到達終止狀態時解除；Kairos 重新啟動後依資料庫重建）。

| | 行為 |
|---|---|
| 抑制的規則 | 離線類：`host_offline`（`sys.host_offline` 與自訂規則）、`host_long_offline`；升級中的主機不計入 `sys.subnet_offline`、`sys.mass_offline` 的台數 |
| 不受影響 | 指標門檻、回報延遲、身分衝突、憑證被拒等其他規則；系統規則「升級推送已暫停」「升級回滾失敗」「發佈簽章驗證失敗」；**已經開著**的離線事件 |
| 期限內 | 不開新事件、不通知（與靜音不同：不留下已靜音的事件）；主機恢復連線後不留任何紀錄 |
| 期限過後仍離線 | 下一輪照常開事件並通知。離線時間以實際離線的時間計（`episode`、「已離線多久」），不重新起算 `grace_secs`；事件的 `details.upgrade`（`rollout_id`、`suppressed_until`〔Unix 毫秒〕）與處理紀錄 `upgrade_suppressed`（`rollout_id`、`until`）註明這台主機是在升級期間離線的 |
| 推送暫停、取消 | 已交給輔助程式的主機照常抑制到期限（替換照常完成） |

抑制只會由主機自己對 Kairos 提議給它的 `upgrade_id` 回報 `applying` 而開始（REMOTE.md 第 13.1 節），一次提議最長 `3600 + 600` 秒；沒有手動開關。

## 6. 通知（§2.1.1、§3.4、§5.4、§6）

| 嚴重度 | 首次 | 未確認時重送 | 升級（需要設定升級對象） | 恢復通知 |
|---|---|---|---|---|
| 緊急 | 所有綁定管道，不排隊、不合併 | 每 15 分鐘（上限 30 分鐘），最多 8 次 | 15 分鐘未確認（使用者規則**必須**設定，否則 `KRS-E5001`） | 一律發送 |
| 嚴重 | 所有綁定管道 | 每 60 分鐘，最多 3 次 | 60 分鐘未確認（選填） | 預設發送 |
| 警告 | **只送 Email**（Webhook 管道可以 `min_severity = "warning"` 改變） | 不重送 | 不升級 | 不發送 |
| 資訊 | 不推播 | — | — | — |

- **管道**：`email`（SMTP；`security` 為 `starttls`〔預設，587〕、`tls`〔465〕、`none`〔不可同時設定密碼〕；AUTH PLAIN／LOGIN；`ca_file` 指定私有 CA；緊急另加 `Importance: high`、`X-Priority: 1`）與 `webhook`（`format`：`kairos`〔`kairos.alert.v1`，§6.6〕、`slack`〔`{"text"}`，Slack／Mattermost／Rocket.Chat 類的 incoming webhook〕、`teams`〔Teams Workflows 的 Adaptive Card〕、`line`〔LINE Messaging API push message，`line_to` 與 `line_token`〕；HMAC-SHA256 簽章 `X-Kairos-Signature: sha256=<hex>`；自訂標頭；只接受 `https://`，內部閘道需要 http 時 `allow_http = true`）。SMTP 與 Webhook 都是純 Rust（hyper 的 client conn、tokio-rustls），沒有新增 crate（只多了 hyper 的 `client` feature 帶入的 `want`、`try-lock`，MIT）。
- **IP**：`show_ip` 為 `full`／`subnet`／`none`；Slack、Teams、LINE 格式預設只顯示網段。
- **機密**：SMTP 密碼、Webhook 網址、標頭、HMAC 金鑰、LINE token 以 AES-256-GCM 加密後存入資料庫（金鑰在資料目錄的 `alerts-secrets.key`，0600）；API 回應、CLI、日誌、稽核紀錄、`kairos db backup` 只有遮蔽值或密文。Webhook 網址顯示為 `https://主機/…`，其餘為 `••••`；錯誤訊息在寫入日誌與發送紀錄前移除機密（`secrets::scrub`）。還原到新主機時要一起複製金鑰檔，否則需重新輸入機密。
- **速率與合併**：每個管道每 30 秒最多 1 則，期間累積的通知合併成一則「另有 N 則」（最多列 10 則）；緊急的第一則不排隊、不合併；彙總事件脫離的子事件同一組合併。
- **重試與退避**：30 秒、1、2、4、5、5…分鐘；429／503 帶 `Retry-After` 時依它；超過 15 分鐘仍無法送出標為 `failed`（記錄錯誤代碼）。
- **機密無法解開**（0.7）：資料目錄的 `alerts-secrets.key` 遺失（Kairos 會建立新的）或與資料庫不成對時，管道的狀態為 `secrets_unavailable`（`last_error_code` 為 `KRS-E5110`）：不會以空的網址或密碼嘗試發送，通知與失敗的管道同樣暫緩；API、`kairos alert-channels list` 顯示狀態，告警服務在狀態轉換時記一次 `WARN`（`alerts.channel.secrets_unavailable`）並發布 `channel_secrets`，「通知管道失敗」以 `KRS-E5110` 開事件。還原金鑰檔（`kairos db restore-keys`）並重新啟動，或重新輸入全部機密後恢復。
- **管道失敗**：連續 3 次失敗 → 管道 `failed` → 系統事件 `KRS-E5107`（改送其他健康的管道）；之後要送到這個管道的通知暫緩（`held`），每 2 分鐘以最舊的一則試送，成功後管道恢復，暫緩的通知不逐則補發、改送一則彙整（最多保留 24 小時）。
- **測試通知**：`POST /api/v1/channels/{id}/test`（或未儲存的 `POST /api/v1/channels/test`）以真實範本與假資料直接送出，回傳結果、回應碼與耗時（S11）。0.7 起限速：同一個管道 60 秒內 3 次、同一個使用者 10 分鐘內 10 次，超過回 429（`KRS-W5111`，`Retry-After`），不送出。

錯誤代碼：`KRS-E5101` 無法連線到 SMTP、`KRS-E5102` SMTP 驗證失敗、`KRS-E5103` 憑證不受信任、`KRS-E5104` 無法連線到外部網路（LINE、Teams）、`KRS-E5105` LINE token 無效、`KRS-E5106` Teams 4xx、`KRS-E5108` Webhook 無法連線或非 2xx、`KRS-E5107` 管道連續失敗、`KRS-E5109` SMTP 伺服器拒絕寄件者、收件者或內容、`KRS-E5110` 管道的設定無效（後兩者由設計包 `decisions/INTEGRATION.md` §28.3 分配）。

## 7. API 與權限

見 [`API.md`](API.md) 第 13 節。每個端點需要的權限只在 `api/alerts.rs` 的 `route` 決定（`ROUTES` 是文件版，測試確認一致），權限與角色的對應在 `kairos/src/rbac.rs`（API.md 第 12.1 節；設計包 `ia.md` §4.2）：

| 權限 | 角色 | 端點 |
|---|---|---|
| `alerts:read`、`silences:read` | 管理員、維運者、稽核員、唯讀 | 事件與靜音的查詢、靜音預覽、SSE 的 `alert.*` |
| `alert-rules:read` | 管理員、維運者、稽核員 | 規則的查詢與試算（唯讀看不到規則） |
| `alerts:write`、`alert-rules:write`、`silences:write`、`channels:read`、`channels:test` | 管理員、維運者 | 確認、指派、暫停、解決、註解；自訂規則與啟停；靜音；管道清單（機密只有遮蔽值）與寄送測試 |
| `alert-rules:admin`、`channels:write` | 管理員 | 系統預設規則的條件；新增、修改、刪除管道（含機密） |

- 只有**全站範圍**的角色指派授予告警的權限；群組範圍的指派看不到告警（保守做法，延後）。
- **0.4／0.5 建立的 API token（`--scope read|operate|admin`）沒有告警的權限**（權限凍結在升級前）：整合改以 `kairos api-token create --role viewer|operator|admin` 建立新 token。
- 寫入（API 與 CLI）都寫入稽核雜湊鏈（`audit_events`，類別 `alerts`；API.md 第 12.8 節），執行者為 `user:<帳號>`、`token:<token_id>` 或 CLI 的作業系統帳號；工作階段的寫入需要 CSRF token。

## 8. CLI

```
kairos alerts list [--view active|needs_me|recovered|all] [--severity <等級>]... [--limit <N>]
kairos alerts show <事件編號>
kairos alerts ack <事件編號>... [--assign-to-me] [--eta <分鐘>]
kairos alerts resolve <事件編號> --root-cause <根因> [--note <說明>]
kairos alerts comment <事件編號> --text <註解>
kairos silences list [--status active|upcoming|ended|all]
kairos silences create --name <名稱> --reason <原因> (--duration <時間> | --until <時間>) [--start <時間>]
                       [--maintenance] [--rule <rule_id>]... [--tag <標籤>]... [--subnet <CIDR>]... [--host <agent_id>]...
                       [--from-event <事件編號>] [--no-end-notify] [--yes]
kairos silences end <silence_id> [--reason <原因>]
kairos alert-rules list | enable <rule_id> | disable <rule_id> --reason <原因> [--yes]
kairos alert-channels list
```

風格依設計包 `cli-style.md`（`--json` 的 `schema` 為 `kairos.<命令>.<動作>/v1`、純 ASCII；本機時區）。確認、解決、註解交給執行中的 Kairos 處理（`alert_actions` 佇列，最多等 10 秒；Kairos 沒有執行時排入佇列，結束代碼 7）。靜音、規則的啟停直接寫入資料庫並寫入稽核紀錄。管道的新增與修改只有 API（需要輸入機密）。

## 9. 效能

`kairos/src/alerts/bench.rs`（`#[ignore]`，與 `api/bench.rs` 相同的方法）：

```bash
cargo test -p kairos --release -- --ignored --nocapture alerts_bench_acks_under_load
```

kp-build（PVE VM，4 vCPU、約 4 GB RAM，release）2026-10-14。1 萬台主機（5,000 台在狀態表中線上），每秒 2,000 筆 report（＝1 萬台 × 每 5 秒一筆），每個階段 30 秒（6 萬筆），量測每筆從放入佇列到收到結果（即連線回 `ack`）的延遲：

| 階段 | ack p50 | ack p99 | ack 最大 | 引擎一個週期 |
|---|---|---|---|---|
| 沒有告警引擎 | 0.55 ms | 1.38 ms | 13.5 ms | — |
| 啟動告警引擎：第一個週期一次開出約 7,500 個事件（CPU > 50% 的 2,500 台、回報延遲 5,000 台），寫入資料庫並通知 Webhook | 0.55 ms | 2.19 ms | 81 ms | 77 ms |
| 穩定期（7,500 個進行中的事件）＋每 10 秒 200 台上下線 | 0.57 ms | 1.23 ms | 11.7 ms | 中位數 104 ms、最大 218 ms |

- 一般運作（穩定期）對 `ack` 的 p99 沒有可量測的影響；一次開出數千個事件時 p99 由約 1.4 ms 增加到約 2.2 ms，最大值 81 ms（大量事件分成每個交易 500 個寫入，兩個交易之間讓 report 的 writer 先寫）。遠低於 `timeouts.write_secs`（5 秒）與協定的 8 秒回應預算。
- 調整前（每個事件以線性搜尋主機、一個交易寫入全部事件）同一個量測的最大值為 239 ms、週期約 600 ms；改為每個週期建立主機索引、沒有生效的靜音時略過比對、分批寫入後如上表。

## 10. 代碼、判斷與未做的項目

**衝刺 5 新增的代碼**（工程暫定，設計包 `decisions/INTEGRATION.md` §28.3 正式分配，號碼不變）：

| 代碼 | 情況 | 用於 |
|---|---|---|
| `KRS-E5109` | SMTP 伺服器拒絕寄件者、收件者或內容（5xx） | 發送紀錄、測試通知的結果 |
| `KRS-E5110` | 通知管道的設定無效（缺少欄位、網址格式、http 未允許等） | `POST/PUT /api/v1/channels` 的 400 `channel_invalid`、發送紀錄 |

沒有分配代碼的新日誌：告警引擎啟動／停止、暖機期結束、保留期限清除、規則或管道重新載入失敗（`WARN`）。

**判斷（可推翻）**：

1. 網段事件成立後等 60 秒才通知（設計的時間軸中網段事件成立當下沒有通知）。
2. 大規模離線另有「至少 10 台」的下限，避免小規模部署 2 台中 1 台離線就發出緊急。
3. Kairos 啟動前就已離線的主機不開主機離線事件（只靠長期失聯），升級時不灌出通知。
4. 警告等級的 Webhook 預設不送（設計寫「只送 Email」），每日 08:30 彙整尚未實作。
5. 系統預設規則的緊急等級在安裝時沒有升級對象：不強制，使用者修改通知設定或建立緊急規則時才強制（`KRS-E5001`）。
6. 範圍不支援群組（清冊沒有群組模型）：帶 `groups` 回 400，不默默忽略。
7. 彙總事件只在範圍不限主機或網段涵蓋時被靜音。
8. 機密以資料目錄的金鑰檔加密（金鑰與資料庫在同一台主機，防的是備份檔與資料庫檔外流）。
9. 停用規則時，它進行中的事件轉為已恢復。

**未做**（之後的版本）：週期性維護時段（每日／每週／每月）、警告的每日彙整、個人勿擾時段與個人管道（需要使用者模型）、指派給他人的個人通知、「加入處理」、資料磁碟剩餘空間（`KRS-W4001`，需要 statvfs）、JSONL 資料檔將滿（`KRS-E4102`）、CRL 自動更新失敗（`KRS-E1205`）、agent 憑證即將到期與過期寬限期即將結束（`KRS-W1110`、`KRS-W1115`）、過期續簽已暫停（`KRS-W2009`）、連線容量規劃不足（`KRS-W0204`，需要授權節點數）、授權（`KRS-E7101`）、主機狀態 `critical`／`warning` 依告警判定（`ia.md` §5.1）、主機離線類規則的歷史試算（需要連線紀錄）、Alertmanager 相容格式、代理伺服器、Email 的 HTML 版。
