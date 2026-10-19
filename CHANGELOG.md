# 變更紀錄

本檔案記錄 Kairos、Pulse 與共用 crate 值得注意的變更，格式依 [Keep a Changelog](https://keepachangelog.com/zh-TW/1.1.0/)。

Kairos 與 Pulse 自 0.2.0 起各自採用 [SemVer](https://semver.org/lang/zh-TW/)：`--version` 印出 `<名稱> <版本> (<commit>)`，發佈包為 `<名稱>-<版本>-<commit>-<os>-<arch>.<格式>`（0.7 以前只有 `linux-x86_64.tar.gz`；0.8.0 起另有 aarch64、deb、rpm 與 Pulse 的 Windows zip／msi）。1.0 之前，破壞性變更升 MINOR（0.2 → 0.3），其餘升 PATCH。0.2.0 之前的建置一律顯示 `0.1.0 (<commit>)`，只能以 commit 區分。版本規則、打 tag 與發佈流程見 [RELEASING.md](RELEASING.md)（只在原始碼 repo 中）。

各項後的代號（例如 S1、A11）對應商業審計的項目編號。

## 版本相容矩陣

Kairos 與 Pulse 混用版本時的相容性。「0.1.0」泛指 0.2.0 之前以 commit 識別的所有建置，各 commit 之間的細節見各產品 README 的「升級注意事項（與 … 混用版本）」。

**協定版本**由每條連線的 hello 協商（`crates/kairos-proto/PROTOCOL.md`「版本協商」），與產品版本號無關：

| 產品版本 | 支援的協定 | hello／hello_ack |
|---|---|---|
| Kairos 0.1.0 | v1 | 只檢查 hello 的 `proto == 1`，忽略不認得的欄位 |
| Kairos 0.2.0 | v1～v2 | 取 client 宣告的範圍與 1～2 中最高的共同版本；v1 時的 `hello_ack`、report 與錯誤處理與 0.1.0 相同 |
| Pulse 0.1.0 | v1 | `proto: 1` |
| Pulse 0.2.0 | v1～v2 | `proto: 1`（讓 0.1.0 的 Kairos 接受）加上 `proto_min: 1`、`proto_max: 2`、`features: ["metrics"]` 與主機資訊；依 `hello_ack` 的 `proto` 與 `features` 決定 report 格式 |

| Kairos ＼ Pulse | Pulse 0.1.0（協定 v1） | Pulse 0.2.0（協定 v1～v2） |
|---|---|---|
| **Kairos 0.1.0**（協定 v1） | 協定 v1，依舊版說明 | **相容，協定 v1**：Kairos 忽略 v2 欄位、回 `"proto":1`，Pulse 改送三欄位的 report；JSONL 維持舊格式（沒有 `schema`），沒有主機資訊。注意：`572b955` 之前的 Kairos 把 `timeouts.write_secs` 設得大於約 8 秒時，寫檔緩慢期間 Pulse 可能誤判斷線；Pulse 的預設心跳改為 15 秒，離線判定變為約 75 秒（破壞性變更第 3 項） |
| **Kairos 0.2.0**（協定 v1～v2） | **有條件相容，協定 v1**：`hello_ack`、report、錯誤處理與 0.1.0 的 Kairos 相同；JSONL 為 `schema` 1（欄位與舊版相同，只多 `schema`），`agents.json` 的 `proto` 為 1、沒有主機資訊。限制：`44dcee5` 之前的 Pulse 在主機名稱含格式字元（Cf）時、`44dcee5` 在主機名稱含 U+034F、Hangul filler、變體選擇字元或私用區字元時**無法註冊**；舊版 Pulse 的讀取閒置計算方式在心跳較大時可能誤斷線 | **相容（建議組合），協定 v2**：report 為通用指標（JSONL `schema` 2），hello 帶主機資訊（寫入 `agents.json`），`hello_ack` 帶 `server_time`（Pulse 檢查時鐘偏差） |

- 同一個 Kairos 0.2.0 可以同時服務 v1 與 v2 的 Pulse：每個 agent 依自己的連線協商，同一個 `reports-*.jsonl` 中 `schema` 1 與 2 的行並存（讀取工具須依 `schema` 分流，見破壞性變更第 6 項）。
- 0.3.0 的協定版本與 0.2.0 相同（v1～v2），另以 feature `durable` 協商 at-least-once 投遞：Kairos 0.3.0 與 Pulse 0.3.0（啟用本機緩衝，預設）都宣告時才啟用，其他組合與上表的 0.2.0 相同（at-most-once）。Kairos 0.3.0 另改為 SQLite 儲存，讀取資料的工具受影響。各組合見 [0.3.0] 的「相容性」。
- 0.5.0 的協定版本仍為 v1～v2：8443 另以 feature `renew`、`deregister` 協商憑證輪替與除役（只在雙方都宣告時使用），註冊是另一個通道（8444 HTTPS，`crates/kairos-proto/ENROLL.md`）。0.4 與 0.5 的任何組合都能以 8443 照常連線；各組合見 [0.5.0] 的「相容性」。
- 0.6.0 的協定與 8443 的 feature 都與 0.5.0 相同，0.5 與 0.6 的任何組合都能連線；Kairos 的資料庫升級到 schema v6（告警、使用者與稽核），Pulse 只新增可觀測性（日誌格式、syslog、本機指標、`diag`）。
- 〔草稿〕0.9.0 的協定版本、8444 的 API 與 Kairos 的 schema（v8）都與 0.8.0 相同，8443 另以 feature `cluster` 協商節點名稱與關機排空（雙方都宣告時才使用），0.8 與 0.9 的任何組合都能連線（各組合見 [0.9.0] 的「相容性」）。
- 0.1.0 與 0.2.0 的任何組合都有共同的協定版本，不會出現 `unsupported_version`；它只會在日後某一端不再支援 v1（例如只支援 v3 以上）時出現。
- **升級順序**：協定上任何順序都可以，混用期間自動以 v1 通訊，不必同時升級。仍**建議先升級 Pulse、再升級 Kairos**，原因是上表「Kairos 0.2.0 × Pulse 0.1.0」的限制（舊版 Pulse 的主機名稱與讀取閒置問題）只在先升級 Kairos 時出現；反過來的組合只有第 3 項的離線判定時間改變。資料格式則在 Kairos 升級的那一刻改變（JSONL 開始有 `schema`），與 Pulse 的升級順序無關。從 0.2.0 升級到 0.3.0 時沒有這些限制，**建議先升級 Kairos、再升級 Pulse**（Pulse 升級後的第一次斷線就能補送），步驟見 [0.3.0]。
- 驗證方式：`kairos-proto` 以凍結的 v1 型別定義驗證舊端忽略 v2 欄位；Kairos 整合測試以 v1／v2 client 涵蓋各種協商結果；Pulse 單元測試以記憶體內的 v1／v2 伺服器驗證 report 格式的選擇；端對端測試（`tests/e2e`）以實際的執行檔驗證 0.2.0 × 0.2.0（v2），並以送出 0.1.0 Pulse 訊息的假 client 驗證 0.2.0 的 Kairos × 0.1.0 的 Pulse（v1）。沒有以 0.1.0 的執行檔實測。

## 支援期限

> 以下為待使用者決定的草稿，數字皆為建議值，尚未對外承諾。

| 項目 | 建議 |
|---|---|
| 支援的版本線 | 最新的 MINOR 版本線（目前 0.8.x）提供錯誤與安全性修正（PATCH） |
| 前一個版本線 | 新的 MINOR 版本發佈後〔待定：建議 6 個月〕內只提供安全性修正，之後終止支援 |
| PATCH 版本 | 同一 MINOR 版本線內不含破壞性變更，可直接升級 |
| Kairos／Pulse 混用 | 保證相鄰兩個 MINOR 版本線可以混用（例如 Kairos 0.3 搭配 Pulse 0.2），以便分批升級〔待定〕 |
| 終止支援通知 | 至少提前〔待定：建議 3 個月〕在本檔案與發佈公告中通知 |
| 平台 | Linux x86_64、aarch64（musl 靜態連結；aarch64 自 0.8.0 起）、systemd 247 以上；Pulse 另有 Windows x86_64（0.8.0 起，**未在 Windows 實機驗證前不列入支援**）；macOS 不提供 |
| 1.0 之後 | 另訂長期支援（LTS）政策〔待定〕 |

## [Unreleased]

## [0.9.0] - 〔草稿：發佈日期待定〕

**Kairos 0.9.0、Pulse 0.9.0〔草稿〕**：衝刺 8——**指標轉出**（商業審計 F7，`kairos/EXPORT.md`：Prometheus exporter、OTLP/HTTP 推送、Grafana 儀表板）、**個資保護**（商業審計 S6，`kairos/PRIVACY.md`：`[privacy]`、刪除主機資料、個資匯出、DPA 範本骨架）與衝刺 4 延後的小項（手動簽發憑證的匯入、8444 的下載端點、`pulse enroll` 送出申請前的 429 等候、過期寬限期 #14 的端對端測試），**HA 的 L4**（審計 A2，`kairos/HA.md`：Pulse 的多個伺服器位址 `server.fallback_addresses`、協定 feature `cluster` 與關機排空 `drain`），另有高可用與集中設定／遠端升級（F8，`crates/kairos-proto/REMOTE.md`）的規格。**協定版本不變（v1～v2）**，新增 feature `cluster`（雙方都宣告時才使用），8444 的 API 不變，0.8 與 0.9 的任何組合都能連線；Kairos 的資料庫**仍為 schema v8**。

0.9.0 的範圍（2026-10-19 使用者決定，HA.md 第 21 節、REMOTE.md 第 14 節全部採建議）另包含 **F8 MVP**（REMOTE.md D15）與 HA 的 **L1a 儲存抽象**，實作會在之後的分支加入（本草稿尚未包含；HA 的 **L4 Pulse 多位址**已包含）；PostgreSQL 後端與叢集協調延到 0.10.0。

### 破壞性變更與升級步驟（升級前必讀）

| # | 產品 | 變更 | 誰會受影響 | 升級步驟 |
|---|---|---|---|---|
| 1 | Kairos | **行程排行的命令列預設遮蔽秘密**（`privacy.process_command` 預設 `redact`，行為變更）：Pulse 開啟完整命令列（`processes.command = "full"`）的主機，命令列中的密碼、token、帶帳號密碼的網址在保存前遮蔽（與診斷包相同的規則，另加 `--password x`、`-pSECRET`；`-p` 開頭的單一破折號參數一律遮蔽）。主控台、API、匯出、指標轉出都只看到遮蔽後的值 | 在 Pulse 開啟完整命令列、需要原樣檢視命令列的部署（Pulse 預設只送執行檔，不受影響） | 需要原樣保存時在 `kairos.toml` 設定 `[privacy] process_command = "keep"` |
| 2 | Pulse | **`pulse enroll` 在送出申請之前收到 429／503 時等候後重試**（依 `Retry-After`，合計不超過 `--wait`，沒有指定時 600 秒）：0.8 直接結束（結束代碼 6） | 依結束代碼 6 立即自行重試的部署腳本 | 腳本不需要自行重試；需要立即失敗時加 `--no-wait` 或較短的 `--wait <秒>` |

**升級步驟**：資料庫 schema 不變（v8），不需要遷移；仍建議先 `kairos db backup <檔案> --include-keys --password-stdin < <密碼檔>`（0.9 起一併備份 `data/privacy.key`）→ 換上 Kairos 0.9.0（tar.gz 照 INSTALL 第 11 節換執行檔；deb／rpm 直接升級）→ 換上 Pulse 0.9.0。指標轉出與 `[privacy]` 的假名化、網段預設都不啟用；需要時依 EXPORT.md、PRIVACY.md 設定。

### 相容性

- 協定（v1～v2）、8444 的 API、schema（v8）都不變；8443 新增 feature `cluster`（雙方都宣告時才使用），0.8 與 0.9 的 Kairos／Pulse 任意組合都能連線（下表）。
- 降版到 0.8 的 Pulse 會忽略 `server.fallback_addresses`，只連 `server.address`。
- **注意**：所有候選位址必須屬於共用同一個資料庫的同一個叢集，durable 的去重才是全叢集的。0.9.0 還沒有共用資料庫的叢集模式（PostgreSQL、租約為 0.10.0），把備用位址指到另一個各自有 SQLite 的 Kairos 時，換節點時在途的 report（最多 4 筆）可能兩邊各寫一次。
- 8444（註冊、輪詢、過期續簽）不跟著候選位址走，仍只用 `enroll.address`（空字串時為 `server.address` 的主機）；叢集部署請設為負載平衡器或虛擬 IP 的名稱。DNS SRV（`server.srv`）未實作（HA 規格的階段 B 選用）。
- 新權限 `export:write`、`hosts:purge`、`hosts:personal_data` 只給內建的管理員（`Permission::ALL` 為 38 個）；0.4／0.5 的舊範圍 token 不會取得（權限凍結）。維運者看不到 S21「整合」分頁（維持 D16）。
- 預設值下的行為與 0.8 相同，唯一的差異是 `process_command = "redact"`（破壞性變更第 1 項）；Prometheus exporter 與 OTLP 推送預設停用；feature `cluster` 只在 Kairos 0.9 × Pulse 0.9 時使用（節點名稱、Kairos 優雅關機時的 `drain`）。
- 指標轉出的設定存在既有的 `settings` 表，不新增資料表；`[privacy]` 在資料進入時轉換，主控台、API、匯出與指標轉出都只看到轉換後的值。

| Pulse ＼ Kairos | Kairos 0.8（以前） | Kairos 0.9 |
|---|---|---|
| **Pulse 0.8（以前）** | 不變 | 與 0.8 相同：不宣告 `cluster` 的 client 收到的 `hello_ack` 不變，也不會收到 `drain`（Kairos 關機時照舊直接關閉連線） |
| **Pulse 0.9** | 舊版 Kairos 不列出 `cluster`：沒有節點名稱、不會收到 `drain`；`fallback_addresses` 照常運作 | 協商 `cluster`：節點名稱、關機排空 |

### Kairos：指標轉出（F7；`kairos/EXPORT.md`）

- **Prometheus exporter**：`GET /api/v1/export/prometheus`（主控台的監聽，HTTPS）以 Prometheus text 0.0.4 或 OpenMetrics 1.0（依 `Accept`）輸出所有可見主機的最新指標。以 API token 驗證、權限 `hosts:read`（viewer 角色即可；群組範圍的使用者只看得到自己的群組，另可 `?group=` 分群抓取）。每個序列帶 `agent_id`、`hostname`、`host_group`；另有 `kairos_host_up`、`kairos_host_info`（顯示名稱、版本、作業系統、標籤）、`kairos_host_last_sample_timestamp_seconds`、`kairos_exporter_hosts`。離線或最後一筆超過 `stale_after_secs`（預設 300）的主機不輸出指標；行程排行預設不輸出（`include_processes`）。預設**停用**（`[export.prometheus] enabled`，未啟用時 `404 export_disabled`）。
- **OTLP/HTTP 推送**：`[export.otlp]` 把每一筆保存成功的 report 以 protobuf 推送到 OpenTelemetry collector：批次（`max_batch_points`）、有上限的佇列與丟棄策略（`queue_max_points`、`drop_policy`）、重試（429／502／503／504 與連線失敗，指數退避、`Retry-After`）、TLS（作業系統的 CA＋`ca_file`、可帶用戶端憑證）、自訂標頭（`headers_file` 或主控台輸入，加密保存）、resource 屬性（`host.name`、`host.id`、`host.arch`、`os.type`、`kairos.host.*`、`resource_attributes`）。名稱預設對應 OpenTelemetry semantic conventions（`naming = "semconv"`：`system.cpu.utilization`、`system.memory.usage{system.memory.state}`、`system.network.io{network.io.direction}`…），可改回 Kairos 的名稱。不支援 gRPC 與 gzip。預設停用。
- **Grafana**：儀表板 `kairos/packaging/grafana-kairos-hosts.json`（Prometheus 資料來源；群組與主機變數、CPU、記憶體、負載、磁碟、網路、運作時間、主機清單）與抓取範例 `kairos/packaging/prometheus-export.example.yml`，兩者與 `EXPORT.md` 一起放進發佈包。
- **主控台**：S21「系統設定 → 整合」新增「指標轉出」卡片：Prometheus 的最近一次抓取、OTLP 的佇列／送出／丟棄／最近一次成功與錯誤；管理員可修改啟用、過期門檻、行程排行、OTLP 的啟用／端點／間隔／名稱／標頭（值只能更換不能檢視）與測試連線。設定存在資料庫 `settings` 表、優先於 `kairos.toml`，環境變數鎖定（`409 locked_by_environment`），寫入稽核（`export.change`、`export.otlp_test`）。
- **API**：`GET`／`PUT /api/v1/export/settings`、`POST /api/v1/export/otlp/test`（API.md 第 16 節）；新權限 **`export:write`**（只有管理員；0.4／0.5 的舊範圍 token 沒有）。
- **`/metrics`**：`kairos_export_prometheus_enabled`、`kairos_export_prometheus_scrapes_total`、`kairos_export_otlp_enabled`、`kairos_export_otlp_queue_points`、`kairos_export_otlp_sent_points_total`、`kairos_export_otlp_requests_total{result}`、`kairos_export_otlp_dropped_points_total{reason}`、`kairos_export_otlp_last_success_timestamp_seconds`。
- **依賴**：沒有新增 crate（OTLP 的 protobuf 為手寫編碼，HTTP 用戶端沿用 hyper 與 tokio-rustls）；資料庫 schema 不變（v8）。
- **量測**（`export_bench`，10,000 台、每台 43 個指標）：Prometheus 輸出 359 ms、56.9 MB；保存最新值約 7 KB／台；OTLP 編碼約 83 萬點／秒（EXPORT.md 第 5 節）。

### Kairos：個資保護（S6；`kairos/PRIVACY.md`）

- **`[privacy]`**（Kairos）：`store_peer_ip`（`full`／`truncate`：IPv4 /24、IPv6 /48／`none`）、`hostname`（`keep`／`pseudonymize`：以 `data/privacy.key` 的 HMAC 換成 `host-xxxxxxxxxxxx`，顯示名稱 `name-…`；不可逆）、`process_command`（`keep`／`redact`／`drop`）、`process_user`（`keep`／`pseudonymize`／`drop`）、`inventory_network`（`keep`／`truncate`：位址 /24、/48、MAC 只留廠商碼／`drop`）。在 8443 的連線與 8444 的註冊申請進入時一次轉換，狀態表、`agents.json`、清冊、report、資產、告警、JSONL 鏡像、主控台與匯出都只看到轉換後的值；日誌、稽核日誌與註冊申請的來源 IP 不受影響（PRIVACY.md 第 2.3 節）。要求假名化而金鑰無法建立或讀取時 Kairos 不啟動。
- **刪除主機資料**：主控台「主機詳情 › 更多 › 刪除主機資料」（只有管理員，新權限 `hosts:purge`；主機須離線；影響預覽＋L2 輸入主機名稱）、`GET/POST /api/v1/hosts/{agent_id}/purge`（`impact_token`）、`kairos hosts purge <agent_id> [--dry-run]`（實際刪除需停止 Kairos，執行中以 `KRS-E4304` 拒絕）。刪除 report、行程快照、彙總、資產與變更歷史、告警事件（含處理與發送紀錄）、註冊申請、憑證紀錄與清冊；先以 `cessationOfOperation` 撤銷這台主機的有效憑證，已撤銷未過期的憑證紀錄保留到過期後 1 天（CRL 需要）；**稽核日誌不刪除**（雜湊鏈），另記一筆 `host.purge`。執行中由 report 的 writer 逐步刪除（每一步 5,000 列，與 report 交錯），刪除後 10 分鐘內拒絕該主機的寫入，避免被建立回來。
- **個資匯出**（資料主體存取請求）：`GET /api/v1/hosts/{agent_id}/personal-data`（新權限 `hosts:personal_data`，只有管理員）、`kairos hosts export <agent_id> [--out]`、主控台「更多 › 匯出個資（JSON）」，`kairos.host.personal-data/v1`：清冊、資產、行程快照摘要、report 的筆數與範圍、告警事件、註冊申請、憑證紀錄、相關的稽核紀錄；每次記稽核。
- **`kairos hosts pseudonym <名稱>…`**：以金鑰算出假名（「`ann-laptop` 在 Kairos 中叫什麼」）。
- **與指標轉出的串接**：刪除主機資料時一併丟棄 Prometheus exporter 在記憶體中的最新值與 OTLP 佇列中這台主機尚未送出的資料（之後的抓取不再輸出）；轉出的主機名稱、顯示名稱與行程排行都是 `[privacy]` 轉換後的值（EXPORT.md 第 2.7 節、PRIVACY.md 第 1 節 #16）。已被 Prometheus 抓取或已推送到 collector 的資料需在該系統處理。

### Pulse 的多個伺服器位址與協定 feature `cluster`（HA L4；`kairos/HA.md` 第 8 節、PROTOCOL.md「叢集與排空」）

- **Pulse `server.fallback_addresses`**：主位址連不上時依序改試的其他 Kairos 節點（"主機" 或 "主機:埠"，IPv6 加方括號，最多 16 個；環境變數 `PULSE__SERVER__FALLBACK_ADDRESSES` 以逗號分隔）。新的鍵、`address` 維持單值，降版安全；安裝字串不變。一輪從主位址開始依序嘗試，收到 `hello_ack` 之前失敗就立即改試下一個（`PLS-W2301`，不增加退避），整輪都失敗才依 `[retry]` 退避（`PLS-E2302`，列出每個位址最後的錯誤）；連上後不主動切回主位址。名稱解析出多個 IP 時依序嘗試，每個最多 3 秒（整個位址仍是 5 秒）。所有位址都以同一個 `server.domain` 驗證伺服器憑證。一次性連線（`pulse enroll` 的確認、輪替後的確認、除役通知、過期憑證的報到）從最近一次連上的位址開始依序嘗試。
- **feature `cluster`**（PROTOCOL.md「叢集與排空」）：Pulse 0.9 一律宣告；Kairos 只在 client 也宣告時才在 `hello_ack` 列出並帶 `server_node`（新設定 `[cluster] node_id`，預設為主機名稱的第一段）。`cluster_id` 留給叢集模式，單機的 Kairos 不送。新增排空訊息 `drain`（Kairos → Pulse）：Kairos 0.9 在**優雅關機**時先對協商了 `cluster` 的連線送 `drain`（`reconnect_within_ms` = `max(10000, 線上連線數 × 5)`）再 `close_notify`，記 `KRS-I8507`；Pulse 收到後停送新的 report、等在途的回應（最多 8 秒）後關閉，在隨機延遲後改連下一個候選位址，不算連線失敗（`PLS-I8507`）。
- **狀態與可觀測性**：`status.json`／`pulse status --json` 的 `connection.server` 改為實際連上的位址，另有 `server_node`、`candidates`、`candidate_index`、`candidate_errors`（只新增欄位，schema 不變）；`pulse status` 顯示「已註冊到 <位址>（Kairos 節點 <名稱>；第 N 個位址，共 M 個）」。本機指標新增 `pulse_server_candidates`、`pulse_server_candidate_index`、`pulse_connect_failovers_total`、`pulse_drains_total`。`pulse check-config` 列出備用位址，`pulse diag` 新增 `candidates.json`（每個位址的名稱解析與 TCP 連線）。
- **Kairos 離線原因 `server_lost`**：服務這台主機的叢集節點失聯（叢集模式由 leader 標記）。0.9.0 只定義名稱：主機 API 的 `offline_reason` 篩選、`/metrics` 的 `kairos_agent_disconnects_total{reason}`（多一個值為 0 的序列）與 Webhook 的固定代碼；單機的 Kairos 不會產生。接收端應忽略不認得的值。

### Kairos：註冊與憑證（衝刺 4 延後的小項）

- **手動簽發憑證的匯入**（PKI.md 第 16 節）：`kairos pki certificates import <檔案或目錄>… [--agent-id]`、`POST /api/v1/pki/certificates/import`（`pki.queue:manage`）、主控台「憑證 › 匯入憑證」。外部 CA（含既有的 RSA 根 CA）或 Kairos 簽發 CA 直接簽發的既有用戶端憑證加入簽發紀錄，檢查不通過以 `KRS-E1305` 列出原因；清冊沒有的主機新增最小的一列。外部 CA 簽發的憑證在 Kairos 撤銷後，8443 在 hello 時拒絕（`KRS-W1105`）。
- **8444 的下載端點** `/agent/<檔名>`（PKI.md 第 17 節）：`enroll.download_dir` 提供 Pulse 的發佈檔案（只有一般檔案、檔名白名單、不跟隨符號連結；內容類型、`nosniff`、`Content-Disposition`；`SHA256SUMS*` 與 `pulse-install.sh` 為 `no-cache`，其他 1 小時；`ETag`／304；串流）。L11 同時下載數（429，`Retry-After: 15`）。主控台的 `downloads` 回報實際狀態，「建立 token」在可用時顯示一行安裝指令。

### Pulse

- **`pulse enroll`**：送出申請之前（`server-info`、`token-check`）收到 429／503 時依 `Retry-After × U(1.0, 1.2) + U(0, 2 秒)` 等候後重試（合計不超過 `--wait`，沒有指定時 600 秒；互動時顯示倒數，Ctrl+C 取消）。0.8 直接結束（結束代碼 6）。
- **`pulse-install.sh --ca-file <根 CA>`**：以 Kairos 的根 CA 驗證下載端點的 HTTPS（完整性仍以發佈公鑰驗證）。

### 主控台（設計包 `decisions/INTEGRATION.md` §31）

- 主機的最後中斷原因新增 `server_lost`「Kairos 節點失聯」（0.9.0 的單機 Kairos 不會產生）；不認得的 `offline_reason` 一律顯示「其他原因」，原始值在技術細節。Webhook 等接收端也應忽略不認得的值。
- SSE `resync` 依原因顯示 Banner（重新啟動、切換節點〔0.10〕、離線太久），主機清單、總覽與告警事件自動重新載入。
- S21 指標轉出卡片、S05 刪除主機資料與假名化說明、S12 下載目錄不完整（只對管理員）、S14 匯入憑證的文案依 §31 定案；`error-codes.js` 依設計包重新產生（Kairos 152 個代碼）。

### 共用 crate：遠端管理的協定與信任鏈（F8 S1；PROTOCOL.md「遠端管理」、`crates/kairos-proto/REMOTE.md`）

- **協定**：REMOTE.md 第 2 節搬入 PROTOCOL.md「遠端管理（feature `config`、`upgrade`）」，範例改為 `kairos-proto` 的測試向量。新訊息 `config_get`／`config`／`config_status`、`upgrade_get`／`upgrade_offer`／`upgrade_chunk_get`／`upgrade_chunk`／`upgrade_status`，hello 的選用物件 `install`（`method`、`upgrade`、`config`），`hello_ack` 與回應心跳的 `ack` 的 `pending`（`config_rev`、`upgrade_id`），新錯誤碼 `upgrade_unavailable`，`rate_limited` 另帶 `retry_after_ms`。兩個 feature 都是雙方宣告才使用；**Kairos 與 Pulse 0.9.0 目前都不宣告**（K1／K2／P1／P2 接上行為後才宣告），因此線上行為與 0.8 完全相同，8443 收到這些 `type` 照舊回 `unknown_type`。
- **`kairos-proto::mgmt`**：上述訊息的型別與驗證、協商（`MgmtSession`、`ServerMgmtPolicy`、`client_mgmt_features`）、`Pending::for_session`、設定的正規化 JSON 與雜湊（`canonical_json`、`settings_hash`）。**設定白名單** `mgmt::settings`：28 個可下發的鍵的型別、範圍、套用方式（即時／重新連線）、最低 Pulse 版本、隱私規則（`processes.command` 與資產序號、網路位址只能下發更保守的值）與合併方式（`collect.interface_exclude` 取聯集），以及兩端共用的 `validate_profile`（Kairos `KRS-E0502`）、`check_remote`／`merge_key`／`needs_reconnect`（Pulse）、`overlay`／`filter_for_version`（Kairos）。
- **`kairos-proto::release`**：`ssh-keygen -Y sign` 簽章（SSHSIG）的驗證與 `allowed_signers` 的解析（只接受 `ssh-ed25519`、`namespaces`／`valid-after`／`valid-before`，其他選項與 `cert-authority` 整行拒絕），以 ring 的 Ed25519／SHA-512 實作、**不呼叫系統的 `ssh-keygen`**；`SHA256SUMS` 的解析與比對、發佈檔名 `<名稱>-<版本>-<commit>-<os>-<arch>.<格式>`、發佈清單 `pulse-<版本>-<commit>-release.json` 的型別（`min_kairos`、`min_upgrade_from`、`remote_features`、`unit`、`breaking`）、SemVer 比較、REMOTE.md 第 4.2.2 節的驗證流程（`verify_bundle`、`check_artifact`，簽章 8401／其他 8402）與降版政策（`check_version_policy`）。測試向量 `crates/kairos-proto/testdata/release` 是以 OpenSSH 9.6 的 `ssh-keygen -Y sign -n kairos-pulse-release` 產生的真實簽章（與 RELEASING.md 第 3 節相同的參數）。
- **發佈清單**（REMOTE.md R1 的清單部分）：Pulse 的打包另產生 `pulse-<版本>-<commit>-release.json`（`packaging/release-manifest.sh`，手動欄位在 `pulse/packaging/release-meta.toml`，unit 的雜湊取自 git 歷史），列在 `SHA256SUMS` 中、受同一個簽章保護；發佈檔由 20 個變為 21 個。`-dirty` 的建置不產生。`remote_features` 目前為空陣列，P1／P2 接上後改為 `["config", "upgrade"]`。
- **依賴**：沒有新增 crate（base64 為 crate 內的實作，Ed25519 與 SHA-512 用既有的 ring）；`Cargo.lock` 不變。
- 為了新的訊息變體，Kairos 與 Pulse 只做了編譯所需的調整：Kairos 對新的 client 訊息一律回 `unknown_type`、`hello_ack` 不帶 `pending`；Pulse 收到新的 server 訊息時忽略（記 DEBUG）、hello 不帶 `install`。

### 其他變更

- `kairos db backup --include-keys` 一併加密備份 `data/privacy.key`。
- 主控台 API 的 `downloads` 另有 `base_url`、`install_script`、`files`、`product_code`；模式的 `stats` 另有 `downloads_in_progress`。
- 註冊服務每小時另外刪除「清冊中已沒有的主機」過期超過 1 天的憑證紀錄。
- 測試掛鉤 `KAIROS_TEST_GRACE_SECS`（debug 建置）：過期寬限期以秒計。
- 內部重構（HA L1a，無行為變更）：Kairos 的儲存層改為領域 store 介面（`Store`／`Tx`），SQL 原封不動搬進 `store/sqlite/`，主控台、註冊、告警、PKI 與所有 CLI 不再直接使用 rusqlite（`kairos/STORAGE.md` 第 17 節）；schema、API 回應、錯誤代碼與 report 的寫入效能不變。

### 規格（尚未實作）

- **`kairos/HA.md`**（審計 A2，高可用）：儲存抽象、PostgreSQL 後端、叢集協調、Pulse 多位址等分層設計與量測計畫；第 21 節的 20 項待決已決定（2026-10-19，採建議）。0.9.0 實作 L1a 儲存抽象與 L4 Pulse 多位址（L4 已包含於本草稿，見上方），PostgreSQL 後端與叢集協調在 0.10.0。STORAGE.md 的「限制與後續」連到 HA.md。
- **`crates/kairos-proto/REMOTE.md`**（F8，集中設定與 agent 遠端升級）：第 14 節 D1～D18 已決定（2026-10-19，採建議）；0.9.0 實作 MVP（D15）。協定部分（第 2 節）已搬入 PROTOCOL.md，共用型別已實作（見上方「共用 crate：遠端管理的協定與信任鏈」）；Kairos 與 Pulse 的行為（K1、K2、P1、P2）尚未實作。

### 新的產品代碼（設計包 `decisions/INTEGRATION.md` §31.3 正式分配）

| 代碼 | 何時 |
|---|---|
| `KRS-E4311` | OTLP 推送失敗（連線、逾時、重試用完） |
| `KRS-E4312` | OTLP collector 拒絕資料 |
| `KRS-W4313` | OTLP 佇列已滿而丟棄資料點（`drop_policy`） |
| `KRS-E4314` | 指標轉出的設定無效（`400 invalid_parameter`） |
| `KRS-E4315` | Prometheus exporter 未啟用（`404 export_disabled`） |
| `KRS-I4204` | 已刪除主機資料（日誌、稽核 `host.purge`） |
| `KRS-E4205` | 無法刪除主機資料：仍連線中（409 `host_online`）、刪除沒有完成、找不到資料 |
| `KRS-E4005` | 要求假名化但 `data/privacy.key` 無法使用，Kairos 不啟動 |
| `PLS-W2301` | 連不上這個候選位址，改試下一個（不增加退避） |
| `PLS-E2302` | 所有候選位址都連不上，依 `[retry]` 退避（列出每個位址最後的錯誤） |
| `PLS-I8507` | 收到 `drain`，改連下一個候選位址 |
| `KRS-I8507` | 關機時對協商了 `cluster` 的連線送出 `drain`（關機日誌） |
| `KRS-W2012` | `enroll.download_dir` 不完整（缺 `pulse-install.sh`、`SHA256SUMS` 或 `SHA256SUMS.sig`） |

新增子類別 23「多伺服器位址與叢集連線」、85「叢集與高可用」（另有 F8 與 0.10 用的 05、44、84）；HA 與 F8 原本都暫定使用子類別 84，§31 把 84 給 F8 遠端升級、叢集改為 85（同尾號），因此排空的代碼由工程暫定的 `KRS-I8407`／`PLS-I8407` 改為 **`KRS-I8507`／`PLS-I8507`**；其餘號碼與工程暫定的相同。Pulse 日誌事件 `conn.failover`、`conn.drain` 的 Windows 事件識別碼為 2005、2007。代碼總數 221 → 287（Kairos 152、Pulse 135，含 0.10 與 F8 的已分配代碼）。

### 已知限制

- OTLP 只支援 HTTP＋protobuf（不做 gRPC、不壓縮）；主機指標名稱沿用 OpenTelemetry 的名稱，不加 `kairos_` 前綴（只有 `kairos_host_*`、`kairos_exporter_*` 與行程排行帶前綴）。
- 假名化不可逆（沒有「顯示原名」）；改變 `[privacy]` 只影響之後收到的資料（PRIVACY.md 第 2.2 節）。
- 刪除主機資料不涵蓋日誌、JSONL 鏡像、備份與已送出的通知；`kairos hosts purge` 需要停止 Kairos。
- 外部 CA 簽發、匯入的憑證撤銷後，已連線中的主機在下一次重新連線時才被拒。
- 刪除主機資料沒有雙人覆核（設計包的選項，之後與其他雙人覆核一起做）。

### 測試

- 指標轉出：`export::tests`（假的 collector：semconv 名稱與 resource、標頭、重試與 `Retry-After`、TLS 驗證、佇列上限與丟棄策略、回填不覆蓋最新值、刪除主機時丟棄、主控台端點與權限）、`export::prom::tests`（OTel → Prometheus 名稱、text 0.0.4 與 OpenMetrics 以 `kp_obs::prom::parse` 嚴格解析）、`export::settings::tests`（環境變數 ＞ 主控台 ＞ 設定檔 ＞ 預設、標頭加密）、`config::export::tests`；端對端 `tests/e2e/tests/export.rs`（實際的 Kairos 與 Pulse、Prometheus 抓取、OTLP 推送到假 collector、Pulse 結束後 `kairos_host_up` 為 0）；主控台 `e2e-export.mjs`。
- 單元：`privacy::tests`（位址截斷、假名、命令列遮蔽、行程與資產的轉換、金鑰檔）、`store::purge::tests`、`privacy_cli::tests`、`pki::queue::import_tests`、`enroll::downloads::tests`、Pulse 的 `rate_limits_before_the_request_are_waited_out`。
- 整合：`integration_tests::privacy_policy_is_applied_before_anything_is_stored`、`api::privacy_tests::purge_and_personal_data_over_https`。
- 端對端（`tests/e2e`）：`privacy.rs`（假名與網段、主控台與 CLI 的刪除、個資匯出）、`enroll.rs` 的 `grace_period_is_enforced_and_an_override_extends_it`（ENROLL.md 第 19.3 節 #14）、`manually_issued_certificate_is_imported_and_revocation_takes_effect`、`download_endpoint_serves_the_release_files`。
- 主控台（Playwright＋axe）：`e2e-privacy.mjs`（S05 的權限、仍連線中、匯出、L2 刪除；S12 一行指令；S14 匯入），`e2e-rbac.mjs` 加入新按鈕與端點。
- Pulse 多位址與 `cluster`：Pulse 的候選清單、輪替與 `drain` 處理、`status`／`check-config`／`diag` 的顯示，Kairos 的 `hello_ack.server_node`、關機排空與 `[cluster] node_id`，`kairos-proto` 的 `cluster` 型別與測試向量；端對端：兩個 Kairos 的換節點與排空。
- 端對端測試的連接埠（`tests/e2e`）：`free_port()` 原本綁定 0 後立即釋放、等 Kairos 之後再綁定，作業系統分配的臨時埠可能在空檔被同時執行的其他測試的連線搶走（偶發 `Address already in use`）。改為從臨時埠範圍之外挑選、同一個測試行程內不重複，並以綁定但不監聽的 socket 佔住到 Kairos 啟動前一刻；Kairos 仍因連接埠被佔用而結束（結束代碼 11）時，等到可以綁定後以相同設定重新啟動（最多 3 次，`start_kairos`）。另修正 `manually_issued_certificate_is_imported_and_revocation_takes_effect` 在回應之後才檢查日誌的競態（改為等待 `KRS-W1105` 出現）。
- 遠端管理的共用型別（`kairos-proto`）：PROTOCOL.md「範例／遠端管理」的 18 則測試向量（含真實的 `SHA256SUMS`、簽章、清單與分段資料，`upgrade_offer` 的向量以測試的 `allowed_signers` 驗證到底）、`mgmt::tests`（協商、`pending` 的容錯、`install`、各訊息的限制、正規化 JSON、白名單與 REMOTE.md 第 3.1 節一致、合併與隱私規則）、`release::tests`（`ssh-keygen` 產生的正確簽章、錯誤 namespace、錯誤金鑰、`hashalg=sha256`、被修改的 `SHA256SUMS`、`valid-after`／`valid-before`、`cert-authority` 與 RSA 等被拒絕的行、客戶重新簽署的 principal、`SHA256SUMS`、檔名、清單、SemVer 先後順序、降版政策）。
- 個資保護 × 指標轉出（整合）：`integration_tests::exported_metrics_only_carry_privacy_transformed_values`（假名化、網段、命令列遮蔽下的 Prometheus 輸出與 OTLP payload 不含原始主機名稱、顯示名稱、完整來源位址、執行身分與密碼）、`api::privacy_tests::purge_and_personal_data_over_https`（刪除後 hub 的最新值與 OTLP 佇列清空）、`e2e-rbac.mjs` 的指標轉出寫入端點 403。

## [0.8.0] - 2026-10-16

**Kairos 0.8.0、Pulse 0.8.0**：衝刺 7 的三條線——**指標擴充**（審計 F4，規格 `crates/kairos-proto/METRICS.md`）、**安裝與平台**（商業審計 O4、O5：`kairos pki`、deb／rpm、aarch64、Pulse 的 Windows 服務與 MSI、一行安裝腳本）與**主控台收尾**（設計包 `decisions/INTEGRATION.md` §29.5 的 #3～#5、§29.6 的「S20 項目併入 `system/health`」、`ia.md` §2／§7 的計數泡泡與全站 Banner、會讓主機無法續簽的設定變更的影響預覽 `impact_token`），另有 mTLS 驗證的安全修正（退役的簽發 CA）。**協定版本不變（v1～v2）**，新增 feature `inventory`、`processes`（雙方都宣告時才使用），0.7 與 0.8 的任何組合都能連線；Kairos 的資料庫升級到 **schema v8**。

### 破壞性變更與升級步驟（升級前必讀）

| # | 產品 | 變更 | 誰會受影響 | 升級步驟 |
|---|---|---|---|---|
| 1 | Kairos | **`PUT /api/v1/pki/policy` 縮短過期寬限期時 `impact_token` 必填**（API.md 第 11.5 節原本就這樣寫，0.7 沒有檢查）：缺少時 400 `confirmation_required`、數字已改變時 409 `impact_changed`；使主機立即無法續簽時 `reason` 必填 | 以腳本縮短寬限期的整合 | 先 `GET /api/v1/pki/policy/impact?expired_renew_grace_days=N`，把回應的 `impact_token`（與 L2 時的 `reason`）一起送出 |
| 2 | Kairos | **「改用設定檔的值」會關閉註冊端點或縮短寬限期時需要確認**：`POST /api/v1/settings/reset` 同上（`GET /api/v1/settings/reset/impact?keys=…`）；`kairos config reset` 有主機受影響時為 L2（互動輸入「停止續簽 N 台」，非互動 `--yes --expect-count <N> --reason <原因>`，數字不符時結束代碼 6） | 以 `kairos config reset enroll.mode` 或 API 改回設定檔的部署腳本 | 腳本加上 `--expect-count` 與 `--reason`（或先查影響再帶 `impact_token`）；其他設定鍵不受影響 |
| 3 | Kairos | **`kairos.service` 多了 `InaccessiblePaths=-/opt/kairos/root-ca`**（`kairos pki init` 的根 CA 暫存目錄對服務不可見；目錄不存在時沒有影響） | 自行把 Kairos 需要讀取的檔案放在 `/opt/kairos/root-ca` 的部署（不應如此） | 不需要動作；換上新版 unit 後 `systemctl daemon-reload` |
| 4 | 兩者 | **發佈檔的種類與檔名增加**：`linux-aarch64`、`.deb`、`.rpm`，Pulse 另有 `windows-x86_64.zip`／`.msi` 與 `pulse-install.sh`；`SHA256SUMS` 的列數增加 | 以檔名樣式抓取 `dist/` 或 `SHA256SUMS` 的自動化（例如只預期 4 個檔案） | 依新的檔名調整；既有的 `<名稱>-<版本>-<commit>-linux-x86_64.tar.gz` 不變 |
| 5 | Kairos | **mTLS 一律拒絕經過已退役簽發 CA 的用戶端憑證鏈**（安全修正；8443 的 agent 連線與 8444 的續簽）：`auth.client_ca` 設為根 CA 或代理 CA 時，0.7 只要用戶端自附中繼憑證，退役（含 `issuing-ca retire --compromised`）的簽發 CA 簽出的憑證仍可通過；0.8 在驗證後另檢查鏈上的每一張 CA 憑證，任何一張是已退役的簽發 CA 就拒絕，與憑證已撤銷相同（`KRS-W1105`；Pulse 端為 `PLS-E1105`） | 以根 CA／代理 CA 當 `auth.client_ca` 的部署中，仍持有退役簽發 CA 所簽憑證的主機（原本就應該已重新註冊） | 不需要動作；建議把 `auth.client_ca` 改為 `data/pki/issuing-cas.pem`（INSTALL 第 7 節、PKI.md 第 15 節），並以 `kairos pki certificates list` 確認沒有主機仍使用退役簽發 CA 的憑證 |
| 6 | Kairos | **資料庫 schema v8**：遷移時重建 `series`（`series_id` 不變），大型資料庫的第一次啟動會多花一些時間；升級後 0.7 的 Kairos 無法開啟 | 所有 SQLite 部署 | 升級前 `kairos db backup`；降版從備份還原 |
| 7 | Kairos | **容量估算由每台約 52 MB 改為約 129.1 MB**（METRICS.md 第 9.6 節、D1）：預設 `storage.max_total_bytes` 64 GiB 約可完整保留 532 台，`server.max_connections` 為預設 1024 時啟動會記容量不足的 `WARN`；系統健康與 `sys.db_capacity` 的預估同步改變 | 主機數多、沿用預設上限的部署 | 依 STORAGE.md 第 10 節調高 `storage.max_total_bytes` 或縮短保留天數 |
| 8 | Kairos | **`GET /api/v1/hosts/{id}/metrics/{name}` 的計數器預設回傳每秒速率**（`transform=rate`），單位 `<原單位>/s`；累計值改用 `transform=none`（只有原始資料解析度）。v8 起計數器的累計值不再彙總 | 以 API 讀取網路流量趨勢的整合（0.7 的主控台沒有使用） | 改讀速率，或加上 `transform=none&resolution=raw` |
| 9 | Kairos | **SQLite 後端的 `hello_ack` 另宣告 `inventory`、`processes`** | 舊版 Pulse 不認得、忽略；比對 `features` 全文的監控腳本 | 無 |
| 10 | Kairos | 趨勢查詢只列出這台主機用過的序列（0.7 會列出所有主機的同名序列，其他主機的為空的 `points`） | 依 `series` 數量判斷的整合 | 無 |
| 11 | Pulse | **`buffer.max_mb` 預設由 64 改為 128**（METRICS.md D7）：每筆緩衝記錄因新指標變大（約 1.5～5 KB），128 MiB 約保存 2～3 天，與 `buffer.max_records` 的約 69 小時相近 | 沒有在 `pulse.toml` 設定 `buffer.max_mb` 的部署：緩衝目錄最多多用約 64 MiB | 磁碟空間不足時在 `pulse.toml` 明確設定 `[buffer] max_mb = 64` |
| 12 | Pulse | **每筆 report 的指標變多**：協商了 `metrics` 時由最多 163 個（磁碟、介面、感測器各 32）改為最多 243 個，**掛載點與介面的上限由 32 改為 16**、磁碟 I/O 裝置 8 | 掛載點或網路介面超過 16 個的主機：優先順序最低的不再送（第一次取樣時記一則 `INFO`，`sysinfo.snapshot`） | 需要時以 `collect.interface_exclude` 排除不需要的介面，或關閉某一類（`collect.*`） |
| 13 | Pulse | **systemd unit 新增 `RuntimeDirectory=pulse` 與 `ExecStartPre=-+/usr/bin/install … /sys/class/dmi/id/product_serial /run/pulse/product_serial`**（METRICS.md D4：以 root 把系統序號複製給 Pulse 讀，Pulse 本身仍不需要特權） | 自行維護 unit 或以 drop-in 覆寫的部署：沒有這幾行時資產不含序號（記一次 `PLS-I8009`） | 換上新版 `pulse.service` 後 `systemctl daemon-reload && systemctl restart pulse`；不想送序號時設定 `[inventory] hardware_serials = false` |

**升級步驟**：先 `kairos db backup <檔案> --include-keys --password-stdin < <密碼檔>`（schema v8 升級後 0.7 無法開啟資料庫）→ 確認 `storage.max_total_bytes` 足夠（第 7 項）→ 換上 Kairos 0.8.0（tar.gz 安裝照 INSTALL 第 11 節換執行檔，換上新版 `kairos.service` 並 `systemctl daemon-reload`）→ 啟動（第一次啟動會遷移 `series`）→ 換上 Pulse 0.8.0 與新版 `pulse.service`（`daemon-reload` 後重新啟動）。以腳本縮短寬限期或改回設定檔的值的整合，依第 1、2 項加上 `impact_token`／`--expect-count`。**不要**把既有的 tar.gz 安裝直接改裝成 deb／rpm（套件的預設設定是全新安裝用的；兩者版面相同，但 unit 改放 `/usr/lib/systemd/system`）。

### 相容性

| Pulse | Kairos | 結果 |
|---|---|---|
| 0.8.0 | 0.1.0（v1） | v1 三欄位，與以前相同 |
| 0.8.0 | 0.2.0～0.7.0 | v2＋`metrics`（＋`durable`）：新指標照送、原樣保存；不送行程排行與資產（舊版不宣告） |
| 0.8.0 | 0.8.0（JSONL 後端） | 同上（JSONL 後端不宣告 `processes`、`inventory`） |
| 0.8.0 | 0.8.0（SQLite） | 全部功能 |

- 0.7 的 Pulse × 0.8 的 Kairos：與 0.7 × 0.7 相同；0.2.0～0.7.0 的 Pulse 在 0.8 的 Kairos 上由磁碟總量與可用量推算使用率（`derived = 2`），主控台標示「推算」。端對端測試涵蓋 0.8×0.8、0.8 Pulse×0.7 Kairos、0.7 Pulse×0.8 Kairos（`tests/e2e/tests/metrics.rs`）。

### Kairos：指標擴充（F4）

- **協定**：依儲存後端宣告 `inventory`、`processes`（JSONL 不宣告）；`inventory` 的驗證、每條連線 60 秒的間隔、保存與回應（`invalid_report` 記 `KRS-W2108`）；report 的 `processes` 驗證（型別錯誤 `bad_request`、內容不符 `invalid_report`）與保存。
- **儲存（schema v8）**：計數器的每秒速率序列（重置、重新開機、晚到的資料沒有速率；重新啟動後以原始資料接續）、Pulse 0.2.0～0.7.0 的推算磁碟使用率（`derived = 2`）、每台主機的序列數上限 `storage.max_series_per_host`（預設 2048，超過時新序列不保存並記 `KRS-W4104`）、資產（合併）與變更歷史（IPv6 暫時位址輪換不記）、行程快照（保留期限與容量刪除同原始資料）、`hosts.boot_time`。STORAGE.md 第 15 節。
- **API**：`GET /api/v1/hosts/{id}/inventory`、`…/inventory/changes`、`…/processes`（權限 `hosts:read`）；趨勢的 `transform`、`kind`、`derived`、推算的點；指標清單的 `kind`、`standard`；主機清單的 `inventory`（不含序號）、`os_family`、`boot_time`、`uptime_secs`、`last_report.disk`，篩選 `os_family`、排序 `disk`、`uptime`，facets 的 `os_family`；詳情的檔案系統、置換空間、負載、行程數、網路與磁碟速率、資產（含序號）；總覽 `top_disk`。API.md 第 5.1～5.4、5.9、5.10 節。
- **告警**：指標門檻新增 `disk`（每個掛載點各自觸發與恢復、`mountpoints` 範圍）、`swap`、`load`（每核心）、`network_rx`／`network_tx`（Mbps、`devices` 範圍）；預覽與回測支援新指標並列出沒有回報的主機數。新的內建規則 **`sys.host_rebooted`「主機重新開機」**（資訊、不通知、預設啟用；D5），內建規則共 32 條。ALERTS.md 第 5.3 節。

### Kairos：主控台收尾的後端

- **系統健康併入四項**（§29.6）：`GET /api/v1/system/health` 新增 `issuing_ca`（`KRS-W1304`、`KRS-E1303`、離線簽發的待簽發超過 24 小時）、`enroll`（S20「註冊端點」狀態表：`KRS-E2001`、`KRS-W2009`、`KRS-W2011`、`KRS-W3005`、僅續簽仍有有效 token）、`alerts`（告警引擎沒有在執行為嚴重）、`metrics`（S20「/metrics 端點」列的三種寫法；這次啟動後曾拒絕經反向代理轉送的請求時註明）。整體狀態與各項狀態對所有人相同，這四項的 `summary`／`values` 只給有 `pki:read`／`enroll:read`／`alerts:read`／`settings:read` 的身分（其他身分 `restricted: true`）；總覽的 `health` 同樣省略。
- **影響預覽**：`GET /api/v1/pki/policy/impact`、`GET /api/v1/settings/reset/impact`——K（目前在寬限期內、以新的寬限期計算已經結束、沒有個別延長的主機）、最早受影響的 10 台、N／M／L／R／T（關閉時）、`level`、`confirm_text`（「停止續簽 K 台」）、`impact_token`（5 分鐘）；確認時的等級、原因與數字寫進稽核。
- **`GET /api/v1/enroll/requests` 的 `state`**（`pending_approval`／`approved`，只用於 `view=pending`）：主控台「待核准」計數泡泡的來源。
- **稽核列表的 `meta.integrity`**：最近一次驗證（自我驗證或手動；結果、執行者、斷點）與目前的 HMAC 起點；`audit.verify` 的 `details` 另帶 `hmac`、`hmac_since_id`。

### Kairos：`kairos pki`（O5）

- **`kairos pki init`**：一個指令建立根 CA（ECDSA P-384、`pathlen:1`、10 年）、伺服器憑證（P-256、SAN＝`--name`、EKU `serverAuth`、`CA:FALSE`、825 天）與代理簽發 CA（沿用 `issuing-ca new`／`install` 的程式碼與儲存，自動啟用線上簽發），並寫出 `data/pki/issuing-cas.pem` 作為 `auth.client_ca`。只能執行一次（已有簽發 CA 或任何檔案已存在時結束代碼 11）；寫稽核 `pki.init`；提醒把根 CA 私鑰移到離線保存（`KRS-W1306`）。INSTALL 不再需要手寫 `ca.cnf`（舊流程移到附錄 P）。
- **`kairos pki sign --profile server|issuing-ca|client`**：在 CA 主機簽發伺服器憑證（`--csr` 或 `--new-key`）、代理簽發 CA、用戶端憑證；簽發者可以是 P-256、P-384 或既有的 RSA 根 CA（PKCS#8）。CSR 驗證簽章、擴充欄位一律不複製；CA 私鑰權限不是 0600 時拒絕（`KRS-E1307`）；輸出檔不覆寫；簽發前先在私鑰旁的 `kairos-ca.log`（0600）附加紀錄。
- **`kairos pki gencrl`**：線上模式呼叫主控台 S15「立即產生 CRL」的同一個函式（`PkiManager::regenerate_crl`，寫稽核 `crl.generate`）；離線模式（`--ca-cert`／`--ca-key`）依 `kairos pki revocations export` 匯出的 `kairos-revocations/v1` 產生 CRL，版本單調遞增。
- 規格與設計取捨：`PKI.md` 第 15 節（含待決定事項：既有部署以根 CA／代理 CA 當 `auth.client_ca` 時，退役的簽發 CA 簽出的憑證仍可能經由那一組驗證器通過；根 CA 私鑰的加密保存）。

### 安全修正

- **退役的簽發 CA 不再能經由 `auth.client_ca` 的其他信任錨通過 mTLS**：8443 的用戶端憑證驗證器在 webpki 驗證通過後，另檢查用戶端送出的鏈中每一張 CA 憑證，若與已退役（含 `--compromised`）的簽發 CA 相同（憑證 DER 或主體＋公鑰）即拒絕，以「已撤銷」拒絕（`KRS-W1105`，主控台的「憑證被拒」與撤銷相同；Pulse 端 `PLS-E1105`）。Kairos 由資料庫寫出 `data/pki/issuing-cas-retired.pem`（已退役簽發 CA 的憑證，0644），熱重載監看它：`kairos pki issuing-ca retire` 後數秒內生效，既有連線在重新驗證時中斷；8444 的過期續簽同樣拒絕由已退役簽發 CA 簽出的憑證。`auth.client_ca` 改為 `data/pki/issuing-cas.pem`（只含啟用中的簽發 CA）的部署原本就會拒絕，行為不變。
- **Pulse：安裝字串可以不出現在命令列**：`pulse enroll --install-string-file <檔案|->`（`-` 為 stdin，取第一個非空白行）。安裝字串含註冊 token，`--install-string <字串>` 在執行期間其他帳號可從 `ps`／`/proc/<pid>/cmdline` 看到，也會留在 shell 歷史；仍保留相容，但說明與文件標示不建議。`pulse-install.sh` 改以 stdin 傳給 `pulse enroll`（另新增 `--install-string-file <檔案>`），Ansible role 改用 `stdin:`。

### 主控台（Web）

- **側邊導覽的計數泡泡**（`ia.md` §7）：「待核准」（待核准申請數，已逾期與已核准待簽發的不算）、「系統健康」（警告或嚴重的項目數，有嚴重時用嚴重樣式），數字在連結的可存取名稱中；S13 核准或拒絕後立即更新。
- **全站 Banner**（`ia.md` §2、`design-system/README.md` §6.13）：CRL 與伺服器憑證即將到期或已過期、回報持續無法寫入（`KRS-E4101`）、以未驗證的方式接受 agent 連線（`KRS-W0302`）、告警引擎沒有在執行。嚴重不可關閉；警告可「24 小時內不再顯示」（只記在這個瀏覽器）。頂列狀態燈改寫最重要一項的摘要（不再用「需要注意」）。
- **S20 系統健康**：只讀健康 API，頁首「整體」與頂列狀態燈一致；「/metrics 端點」列附「經反向代理提供 /metrics 時必須設定 token…」（`KRS-W0304`）；簽發 CA 私鑰無法使用改為嚴重；註冊端點列依狀態表；沒有權限的項目只顯示狀態；處置捷徑。
- **S18 稽核日誌**：頁首「完整性：✓ 已驗證至 …（雜湊鏈連續；HMAC 自第 N 筆起）· 系統自動驗證」；驗證通過的第一行改為「N 筆，雜湊鏈連續（第 1～N 筆）」；竄改類斷點的「前往第 N 筆」；HMAC 起點之後卻沒有 HMAC 的紀錄不再寫成「啟用 HMAC 前」；拿掉 Banner 中錯誤代碼登錄表的原文；只看自己操作的維運者沒有完整性區塊。
- **S11 通知管道**：測試通知 429（`KRS-W5111`，依 `product_code` 與驗證失敗的限速 `KRS-W6011` 區分）在按鈕下方就地倒數 `Retry-After`、按鈕 `aria-disabled`、保留上一次結果（清單每列各自顯示）；機密無法解開時狀態為「失敗」、原因「無法解開機密」，「寄送測試通知」停用並說明；刪除對話框的確認按鈕改為「刪除通知管道」。
- **S21 系統設定**：「改用設定檔的值／內建預設值」先預覽影響，會關閉註冊端點或縮短寬限期時列出受影響的主機並帶 `impact_token`，有主機受影響時 L2；套用模式或改用設定檔的值送出時數字已改變（409）會關閉對話框、以新的數字重新確認（0.7 只換 token、沿用舊的確認字串）；「整合」分頁的 /metrics 反向代理說明；模式按鈕的提示「已是目前的模式」、寬限期 0 天的說明指向憑證政策。
- **S12、S13、S16**：註冊端點為僅續簽或關閉時，管理員看到「前往系統設定」、其他角色看到「請管理員…」；批次結果訊息的主機名稱不再重複括號；使用者詳情對自己或最後一位全站管理員的「刪除使用者…」改為 `aria-disabled` 並說明原因。
- **權限的自動掃描**（`tests/e2e-rbac.mjs`）：每個角色逐頁（含清單第一筆的詳情頁）比對看得到的寫入按鈕與 `ia.md` §4.2，沒有分類的寫入按鈕視為失敗，並反向確認有權限的角色看得到主要的寫入按鈕。
- **指標擴充**（F4）：S05 主機概覽的目前數值（置換空間、負載與每核心、使用率最高的掛載點與「將滿」、運作時間、行程數）；趨勢的六個指標群組（掛載點小倍數、忙碌時間比例、網路錯誤數、負載參考線、表格檢視）；「行程」與「資產資訊」分頁（序號只在詳情）；S04 的磁碟欄、作業系統（家族）篩選與欄位設定；S03 的磁碟排行；S09 告警規則的 7 個指標、掛載點／介面範圍與預覽的「沒有回報」；S08 的新範本；舊版 Pulse 與 Windows 的空狀態。

### Pulse：指標擴充（F4）

- **19 個新的標準指標**（共 27 個；METRICS.md 第 3 節）：負載（`system.cpu.load_1m`／`5m`／`15m`，只有 Linux）、可用記憶體、置換空間（總量／已使用）、每個掛載點的已使用量與使用率（`used` = total − free，`usage` 與 `df` 的 Use% 相同；Linux 以 `statvfs` 取 `f_bfree`、Windows 以 `GetDiskFreeSpaceExW`）、磁碟 I/O（累計讀寫量；Linux 另有讀寫次數與忙碌時間，直接讀 `/proc/diskstats`，裝置名稱支援 device mapper 的 `dm-N`）、網路封包與錯誤、行程數（同 `ps -e`）、開機秒數。**計數器一律送原始累計值**，速率由 Kairos 計算。協商了 `metrics` 的 0.2.0～0.7.0 Kairos 只檢查格式、原樣保存。
- **每類上限與優先順序、以整組為單位的截斷**：掛載點 16（`/` 或系統磁碟機優先，其餘容量大者）、磁碟 I/O 裝置 8（依掛載點順序）、介面 16（有 IPv4 → 流量大者）、溫度 32；整行超過 65536 bytes 時依序捨去行程命令列 → 行程排行（先縮成各 3 項）→ 溫度 → 介面 → 磁碟 I/O 裝置 → 掛載點，每組（例如一個掛載點的 4 個指標）一起捨去，無 label 的指標一律保留；每 10 分鐘最多一則 `WARN`。
- **行程排行**（feature `processes`；`[processes]`）：每 60 秒附在 report 上，依 CPU、記憶體各 5 個；CPU 為占整台主機的百分比。命令列預設不送（`processes.command = "none"`），可設 `executable`（只送執行檔路徑）或 `full`（含參數，截斷到 1024 bytes）。行程排行隨 report 存入本機緩衝、斷線後一起補送；0.7 寫入的緩衝記錄仍可讀取。只看得到自己的行程（`ProtectProc=invisible`）時不送行程數與排行，記一次 `PLS-I8009`。
- **主機資產**（feature `inventory`、新訊息 `inventory`；`[inventory]`）：OS、開機時間、CPU（型號、廠商、實體／邏輯核心數、標稱時脈）、記憶體、廠牌型號序號（SMBIOS）、主機板、BIOS、磁碟（掛載點、裝置、檔案系統、容量、SSD／HDD、可移除、唯讀）、網卡（MAC、IP、MTU）。連線後送一次、內容改變才送、距離上次送達滿 24 小時也送；`invalid_report` 後同樣的內容不再送（`inventory.rejected PLS-W2108`），`storage_unavailable` 5 分鐘後重送（資產的速率限制只記 `DEBUG`、沒有代碼）。不經過本機緩衝。
- 設定：`[collect]`（`filesystems`、`disk_io`、`network`、`temperature`、`max_filesystems`、`max_disks`、`max_interfaces`、`interface_exclude`）、`[processes]`（`enabled`、`interval`、`top`、`command`、`include_user`〔本版不實作〕）、`[inventory]`（`enabled`、`interval`、`hardware_serials`、`network_addresses`）。hello 依設定宣告 `inventory`、`processes`；本機狀態檔（`status.json`、`pulse status --json`）的 `features` 列出協商結果。
- 代碼（設計包 §30.3 正式分配）：`PLS-W2108`（資產資訊被伺服器拒絕）、`PLS-I8009`（部分系統資訊無法取得：序號、磁碟 I/O、行程）。事件 `sysinfo.snapshot`、`sysinfo.limited`、`inventory.rejected`（Windows 事件識別碼待分配）。

### Pulse：Windows（O4）

- **Windows 服務**：`pulse service install|uninstall|run`（`windows-service` 0.8.1）。服務 `Pulse`、自動啟動、LocalSystem、失敗時 10 秒後重新啟動（一天最多 3 次）。`service run` 先把工作目錄切換到 `pulse.exe` 所在目錄（或 `--workdir`），**不再落在 `C:\Windows\System32`**；停止與關機要求與 Ctrl+C 相同地送出 close_notify 後結束。新增代碼 `PLS-E8105`、`PLS-E8106`（設計包 §30.3 分配到子類別 81；工程暫定的 `8205`／`8206` 已改號）。
- **MSI**（msitools 0.106 的 `wixl`，在 Linux 建置容器內產生）：安裝到 `C:\Program Files\Pulse\`、註冊服務（不加入 PATH：wixl 的 `<Environment>` 無法重現）；`pulse.toml` 升級與移除都保留；MajorUpgrade。另有內容相同的 zip。
- **未在 Windows 實機驗證**（只在 kp-build 交叉編譯、clippy 與產生 MSI）：服務的安裝與啟停、MSI 的安裝／升級／移除、工作目錄、`pulse enroll` 重新啟動服務、檔案 ACL 都待在 Windows 上測試。Windows 事件記錄、MSI 的安裝字串參數與移除時的 `pulse decommission`、程式碼簽章仍未做。

### 套件與安裝（O5）

- **deb／rpm**（nfpm 2.47.0，在建置容器內產生）：Kairos 與 Pulse 各一個，x86_64 與 aarch64。版面與 tar.gz 相同（`/opt/<名稱>`），加上 `/usr/lib/systemd/system/<名稱>.service`、`/usr/bin/<名稱>`；預設設定為 config（升級不覆寫）；安裝前建立系統帳號，安裝後建立可寫目錄（Pulse 另產生 `agent.id`）、`systemctl enable`（不啟動），升級時 `try-restart`，移除時 `disable --now`；移除與 purge 都不刪除 Kairos 的資料目錄。Kairos 套件的預設設定以絕對路徑指向 `kairos pki init` 寫出的檔案，安裝後只要 `pki init` 與 `systemctl start`（INSTALL 第 A 節）。
- **`pulse-install.sh`**（一行安裝腳本）：以從另一個管道取得的 `allowed_signers` 驗證 `SHA256SUMS` 的簽章 → 依架構與套件管理員選出套件、核對 SHA-256（不接受 `-dirty`）→ 安裝 → `pulse enroll --install-string`（安裝字串可由環境變數傳入）→ 啟動服務。也支援已下載好的目錄（`--dir`，離線環境）。
- **Ansible role（最小版）**：`packaging/ansible/`（role `pulse`、範例 `site-pulse.yml`），做與安裝腳本相同的驗證、安裝與註冊；只經過 syntax-check，尚未對實際主機執行（Pulse 的 INSTALL 第 A.4 節）。Kairos 沒有 role。
- **套件的 systemd unit 與發佈包的 unit 為同一份檔案**（`kairos/packaging/kairos.service`、`pulse/packaging/pulse.service`）：`build-release-common.sh` 打包後從 deb（`ar`＋`tar`）與 rpm（解析 header 後的 cpio）取出 `/usr/lib/systemd/system/<名稱>.service` 與來源檔逐位元比對，不同時建置失敗；`pulse.service` 的序號複製（`ExecStartPre=-+/usr/bin/install …`、`RuntimeDirectory=pulse`）因此也在 deb／rpm 中。

### 平台與發佈（O4）

- **aarch64 Linux（musl）**：Kairos、Pulse 都以 cargo-zigbuild 0.23.4＋zig 0.16.0 交叉編譯，以 qemu-aarch64 在建置容器內執行 `--version` 確認。
- 發佈腳本（`packaging/build-release-common.sh`）改為多目標：`TARGETS`（預設 Kairos 為 Linux x86_64／aarch64，Pulse 另加 Windows x86_64）、`PACKAGES`（預設 `deb rpm`）、`BUILD_JOBS`（容器內 cargo 的平行數，預設 2）。THIRD-PARTY-LICENSES 與 SBOM 每個目標各自產生（Windows 的依賴不同）。
- **可重現**：所有新產物以 commit 時間為時間戳記（tar、zip〔TZ=UTC〕、nfpm 的 `mtime`、MSI〔libfaketime〕），MSI 的 PackageCode／ProductCode 由版本與 commit 推導，Windows 執行檔以 `--no-insert-timestamp` 連結並去掉符號表；`packaging/check-reproducible.sh` 比對所有產物。
- 建置容器多安裝的 Alpine 套件（版本固定）：`zig`、`qemu-aarch64`、`mingw-w64-gcc`、`msitools`、`libfaketime`、`zip`；nfpm 以固定版本下載並核對 SHA-256。

### 共用 crate（指標擴充規格）

- **`crates/kairos-proto/METRICS.md`**〔0.8.0 規格〕：磁碟（每個掛載點的用量與使用率、I/O）、網路（封包、錯誤）、負載、置換空間、行程數、uptime 等 19 個新標準指標（共 27 個），行程排行（feature `processes`）與主機資產（feature `inventory`、新訊息 `inventory`）；採集來源與上限、計數器的重置處理、相容矩陣、隱私、Kairos schema v8 草案、查詢 API、告警、主控台與實作分工。PROTOCOL.md 的標準指標表、features 表、訊息表與錯誤碼表同步更新。**協定版本不變（v1～v2）**；新指標沿用 `metrics` 陣列，0.2.0～0.7.0 的 Kairos 只檢查格式、原樣保存。
- `kairos-proto`：`StandardMetric` 增加 `kind`（量測值／計數器）與 `labels`；`counter_rate`、`filesystem_usage`；`ProcessList` 等與 `ReportBody::processes`；`Inventory` 等、`ClientMsg::Inventory`、`validate_inventory`、`inventory_digest`；`Session::inventory`、`Session::processes`。兩個新 feature 不在 `SERVER_FEATURES` 中（由 Kairos 依儲存後端宣告）。`StandardMetric`、`Session`、`ReportBody` 新增欄位，以結構常值建立它們的程式需要補上欄位。

### 新的產品代碼

設計包 `decisions/INTEGRATION.md` §30.3 正式分配 8 個代碼：6 個號碼與工程暫定的相同，Pulse 的 Windows 服務兩個由工程暫定的 `8205`／`8206` 改為 `PLS-E8105`／`PLS-E8106`（子類別 81「Windows 服務與安裝程式」）。總數 213 → 221（Kairos 108、Pulse 113）；子類別 80 改名為「平台、systemd 與系統資訊」。mTLS 的安全修正沿用 `KRS-W1105`，不新增代碼。

| 代碼 | 等級 | 何時 |
|---|---|---|
| `KRS-W2108` | 警告 | 資產資訊不合法：Kairos 以 `invalid_report` 拒收 `inventory` |
| `KRS-W4104` | 警告 | 主機的指標序列數已達上限（`storage.max_series_per_host`），部分指標未保存 |
| `PLS-W2108` | 警告 | 資產資訊被伺服器拒絕（同樣的內容不再送） |
| `PLS-I8009` | 資訊 | 部分系統資訊因權限或平台限制無法取得（序號、磁碟 I/O、行程），每項只記一次 |
| `KRS-W1306` | 警告 | `kairos pki init` 完成時：根 CA 私鑰仍在這台伺服器上 |
| `KRS-E1307` | 錯誤 | `kairos pki sign`、離線 `kairos pki gencrl`：CA 憑證或私鑰無法使用 |
| `PLS-E8105` | 錯誤 | `pulse service install`／`uninstall`：服務控制管理員回報錯誤（多半是沒有系統管理員權限） |
| `PLS-E8106` | 錯誤 | `pulse service run` 不是由服務控制管理員啟動，或工作目錄無法切換 |

事件 `sysinfo.snapshot`、`sysinfo.limited`、`inventory.rejected`（Pulse；Windows 事件識別碼待分配）。

### 測試

- Kairos：`pki::ca`（根 CA 擴充欄位、伺服器憑證通過 webpki 的 `serverAuth` 與名稱驗證、簽發 CA 通過 `issuing-ca install` 的檢查、既有 RSA 根 CA、CSR 簽章、離線 CRL 與撤銷清單、`pki init` 的覆寫保護與權限）、`pki_cli`（`sign` 的紀錄與不覆寫、私鑰權限、離線 `gencrl` 的版本遞增、用法錯誤）。
- 端對端：`pki_init_then_install_string_registers_and_the_service_connects`——只以 `kairos pki init` 產生的 PKI 啟動 Kairos，Pulse 以安裝字串註冊並以 mTLS 連上；線上 `pki gencrl`；第二次 `pki init` 被拒。
- Pulse：`pulse service` 的參數解析與工作目錄；Linux 與 `x86_64-pc-windows-gnu` 的 clippy。
- 指標擴充：Kairos 的 schema v8 遷移、速率與推算序列、資產與行程的協定處理、新告警指標與回測；Pulse 的各平台採集、截斷與優先順序、資產摘要與重送；端對端 `tests/e2e/tests/metrics.rs`；主控台 `e2e-metrics.mjs`。
- 主控台收尾：`e2e-rbac.mjs` 的寫入按鈕自動掃描、`e2e-admin.mjs`／`e2e-pki.mjs` 的 Banner 與計數泡泡、影響預覽；`run.sh` 在任何失敗、axe 違規、主控台錯誤或沒有執行測試時以非 0 結束。
- 安全修正：8443 拒絕經過已退役簽發 CA 的鏈（`auth.client_ca` 為根 CA、為簽發 CA 本身、用戶端自附中繼憑證、熱重載後既有連線）；8444 的過期續簽拒絕已退役簽發 CA 簽出的憑證；`issuing-cas-retired.pem` 的寫出。

### 尚未實機驗證

- Windows（只經過 `x86_64-pc-windows-gnu` 的交叉編譯與 clippy）：虛擬服務帳號 `NT SERVICE\Pulse` 下的磁碟 I/O（`IOCTL_DISK_PERFORMANCE`）、行程清單與命令列、SMBIOS 序號、`GetDiskFreeSpaceExW`、登錄的 BIOS 與 `~MHz`。
- Pulse 的 Windows 服務與 MSI（見上方「Pulse：Windows」）；Ansible role 只經過 syntax-check。

## [0.7.0] - 未單獨發佈（內容隨 0.8.0 發佈）

**Kairos 0.7.0、Pulse 0.7.0**：衝刺 6 的**主控台網頁介面**（審計 F2：S01、S03～S18、S20、S21、S23）與後端補強——設計包 `decisions/INTEGRATION.md` §28.5 的待辦與 §28.9 #3（金鑰檔的備份）。**協定不變**，Pulse 只有設定檔錯誤代碼的小修正。

### 破壞性變更與升級步驟（升級前必讀）

| # | 產品 | 變更 | 誰會受影響 | 升級步驟 |
|---|---|---|---|---|
| 1 | Kairos | **主機 API 的 `offline_reason` 改用事件匯流排的名稱**：`timeout` → `idle_timeout`、`connection_closed` → `agent_closed`（`server_shutdown`、`certificate_rejected`、`write_unknown` 不變），與 Webhook 的 `offline_reason`、`/metrics` 的 `kairos_agent_disconnects_total{reason}` 相同（§28.5 #4）。SSE `host.offline` 的 `reason` 同樣改變 | 讀取 `status_detail.offline_reason`、`host.offline` 的 `reason`，或以 `?offline_reason=timeout` 查詢的整合 | **過渡期（0.7）**：回應另帶舊名稱的 `status_detail.offline_reason_legacy`、SSE 的 `reason_legacy`、總覽離線清單的 `offline_reason_legacy`；查詢參數新舊名稱都接受。請在 0.7 期間改讀新名稱；`*_legacy` 與舊的查詢值在 0.8 移除 |
| 2 | Kairos | **資料庫升級到 schema v7**（`audit_events.hmac`） | 所有 SQLite 部署 | 升級前 `kairos db backup`；升級後 0.6 無法開啟資料庫（降版從備份還原） |
| 3 | Kairos | **資料目錄多了 `audit-hmac.key`**（稽核鏈的 HMAC 金鑰，0600），服務第一次以 0.7 啟動時建立。之後的稽核紀錄都帶 HMAC；**金鑰遺失或不符時 `kairos audit verify` 以 `KRS-E6201` 失敗（結束代碼 9），並開「稽核日誌完整性」緊急事件** | 備份、還原、搬移資料目錄的部署；監看 `kairos audit verify` 結束代碼的腳本 | 把 `audit-hmac.key` 加進備份（`kairos db backup --include-keys`，或自行加密保存）；還原時與資料庫一起還原 |
| 4 | Kairos | **新增系統預設規則「稽核日誌完整性」**（`sys.audit_integrity`，緊急，`KRS-E6201`，共 31 條），每 24 小時自我驗證一次 | 所有啟用告警的部署 | 不需要動作；需要時以 `audit.verify_interval_hours` 調整頻率 |
| 5 | Kairos | **`/metrics` 在未設定 token 時拒絕帶有反向代理轉送標頭的請求**（`Forwarded`、`X-Forwarded-For`／`-Host`／`-Proto`、`X-Real-IP`、`Via` → 403） | 經同一台主機上的反向代理、未設定 token 抓取 `/metrics` 的部署（原本就不安全） | 設定 `observability.metrics_token_file`，讓 Prometheus 帶 bearer token（INSTALL 9.2a） |
| 6 | Kairos | **寄送測試通知加上限速**：每管道 60 秒 3 次、每使用者 10 分鐘 10 次，超過回 429 | 以腳本大量呼叫 `POST /api/v1/channels/{id}/test` 的整合 | 依 `Retry-After` 重試 |

**升級步驟**：先 `kairos db backup <檔案> --include-keys --password-stdin < <密碼檔>`（同時加密備份三個金鑰檔）→ 換上 Kairos 0.7.0（`kairos --check-config`）→ 啟動後 `kairos audit verify`：0.6 的紀錄只有雜湊鏈，輸出寫出「第 N 筆起有 HMAC」的邊界 → 讀取 `offline_reason` 的整合改用新名稱 → 經反向代理抓 `/metrics` 的部署設定 token。

### Kairos

- **`offline_reason` 名稱統一**（§28.5 #4）：主機列表、詳情、總覽與 SSE 改用 `idle_timeout`／`agent_closed`；過渡期另帶 `*_legacy`，查詢參數接受新舊名稱；`offline_reason` 查詢另接受 `write_unknown`（0.6 漏列）。
- **金鑰檔的加密備份**（§28.5 #8、§28.9 #3）：`kairos db backup --include-keys (--password-file <檔案> | --password-stdin)` 把存在的 `alerts-secrets.key`、`console.key`、`audit-hmac.key` 以備份密碼加密寫成 `<備份檔>.keys`（0600；Argon2id m=64 MiB／t=3／p=1 衍生金鑰＋AES-256-GCM，`format`、KDF 參數與鹽為 AAD），預設不含；不接受命令列上的密碼。`kairos db restore-keys <檔案>` 解開並以 0600 寫回資料目錄（暫存檔＋改名；以 root 執行時擁有者改為資料目錄的擁有者；不同的既有檔案需 `--overwrite`，舊檔改名保留）。密碼錯誤與檔案被修改都以`KRS-E4307` 拒絕。密碼、衍生金鑰與解開的金鑰在記憶體中以 `zeroize` 清除（`zeroize` 已在依賴樹中，經 rustls）。
- **稽核雜湊鏈的 HMAC**（審計 S7 的強化）：每筆紀錄的雜湊另以 `audit-hmac.key` 衍生的金鑰計算 HMAC-SHA256，head 另帶 mac；HMAC 的起點記在 `meta.audit_hmac`，`kairos audit verify`、`POST /api/v1/audit/verify` 寫出邊界與 `hmac` 結果。新的斷點原因 `missing_hmac`、`hmac_mismatch`、`hmac_boundary_mismatch`、`hmac_key_unavailable`、`hmac_key_mismatch`。金鑰與 `console.key` 分開（取捨見 `kairos/src/audit.rs`）。
- **鏈尾外送與自我驗證**（§28.5 #10）：每 `audit.anchor_log_minutes`（預設 60）分鐘寫一行固定事件 `audit.chain_anchor`（`KRS-I6202`），經 syslog 外送成為資料庫之外的副本；每 `audit.verify_interval_hours`（預設 24）小時自我驗證，結果發布到內部事件匯流排（`audit_integrity`），失敗時記 `ERROR` 並開 `sys.audit_integrity`。
- **`/metrics` 與反向代理**：見破壞性變更第 5 項；第一次拒絕時記 `WARN`（`observability.metrics_proxied`，`KRS-W0304`）。
- **通知管道的機密無法解開**：狀態 `secrets_unavailable`（`KRS-E5110`），API、`kairos alert-channels list` 顯示；不嘗試發送；告警服務在狀態轉換時記 `WARN` 並發布內部事件 `channel_secrets`，「通知管道失敗」以 `KRS-E5110` 開事件（原本只在日誌記 `alerts.channel.decrypt_failed`）。
- **測試通知的限速**：每管道 60 秒 3 次、每使用者 10 分鐘 10 次（429，`KRS-W5111`）。
- **啟動時的設定檔錯誤帶代碼**：`錯誤 KRS-E0101／E0401／E0402（或 E0201／E0202）：Kairos 啟動失敗: …`，與 `--check-config` 相同的判斷，結束代碼仍為 3；CLI 的設定檔錯誤同樣改用正確的代碼（原本一律 `KRS-E0201`）。
- **`kairos api-token create`**：沒有 `--role`／`--user` 時在 stderr 提示「i 未指定 --role，使用唯讀（viewer）；需要讀取註冊、憑證與設定時請改用 --role auditor」；`--scope` 改以 `⚠ 警告 KRS-W0104` 提醒（`--json` 放在 `warnings`）。
- 設定：`[audit] anchor_log_minutes`（0 或 5～1440）、`verify_interval_hours`（1～168）。依賴：`zeroize`（直接依賴，版本與 rustls 使用的相同，不新增套件）。

### Pulse

- 啟動時 `pulse.toml` 存在但無法讀取（權限、I/O）改以 `PLS-E0402` 說明（原本為 `PLS-E0101`）。

### 新的產品代碼（設計包 `decisions/INTEGRATION.md` §29.3 正式分配，號碼與工程暫定的相同）

| 代碼 | 意義 | 使用處 |
|---|---|---|
| `KRS-W0304` | `/metrics` 經反向代理轉送、但沒有設定 `observability.metrics_token_file` 而拒絕 | 日誌（第一次） |
| `KRS-E4307` | 金鑰檔的加密備份無法建立、解開或還原（密碼錯誤、檔案被修改、格式不符、無法寫回資料目錄；子類別 43 改名為「匯出、匯入與備份」） | `kairos db backup --include-keys`、`kairos db restore-keys` |
| `KRS-W5111` | 測試通知太頻繁（429 `rate_limited`） | `POST /api/v1/channels/{id}/test` |
| `KRS-I6202` | 稽核鏈尾的定期外送（固定事件 `audit.chain_anchor`） | 日誌、syslog |

沿用的代碼：通知管道的機密無法解開沿用 `KRS-E5110`（§28.5 #8）；HMAC 的各種失敗與自我驗證失敗沿用 `KRS-E6201`。

### 測試

- Kairos：主機 API／SSE 的新舊名稱與查詢相容；`/metrics` 的轉送標頭（六種標頭、空值、有 token 時照常）；管道機密無法解開的狀態轉換（store、引擎、派送、API、內部事件、系統事件）與測試通知的限速（單元與 HTTPS 端點）；稽核 HMAC（重新計算的鏈、金鑰遺失與不符、邊界移動、head 截斷、保留清除、0.6〔v6〕資料庫與鏈升級到 v7）；自我驗證發布事件、竄改後開「稽核日誌完整性」並在通過後恢復；金鑰備份（往返、錯誤密碼、竄改密文／nonce／鹽／KDF 參數、異常的記憶體參數、權限 0600、不覆寫、不只還原一半、`--overwrite` 保留舊檔）；CLI 的參數（拒絕命令列密碼）；設定檔錯誤代碼；`api-token create` 的提示。
- 端對端：`exit_codes.rs` 檢查啟動時的 `KRS-E0401`、`KRS-E0101`、`PLS-E0101`；`api.rs` 檢查 `agent_closed` 與 `*_legacy`。

### 主控台網頁介面

衝刺 6 的**主控台網頁介面**（審計 F2；設計包 `kairos/screens.md`、`ia.md`、`design-system/`、`decisions/INTEGRATION.md` §28.6）：地基與 S01、S03～S05、S23，加上告警（S06～S11）、註冊與憑證（S12～S15）、管理與系統（S16～S18、S20、S21）三組畫面。架構與慣例見 [`kairos/web/README.md`](kairos/web/README.md)（各畫面有與隱藏的元素見第 8 節），伺服器端的規則見 `kairos/API.md` 第 14 節。

#### 地基

- **網頁介面**（Kairos）：與主控台 API 同一個 HTTPS 監聽（預設 `https://127.0.0.1:9443/`）。原生 ES modules＋vanilla JS，沒有第三方套件與 npm 建置步驟；`kairos/build.rs` 在編譯時把 `kairos/web/index.html` 與 `kairos/web/assets/` 嵌入執行檔（依路徑排序，不影響可重現性）。`/assets/…` 帶 `ETag`、`Cache-Control: no-cache`；其他沒有副檔名的路徑回 `index.html`（History API 路由）。
- **畫面**：S01 登入（本機／AD、兩步驟驗證的驗證碼或復原碼、第一次登入變更密碼、鎖定剩餘時間、工作階段已結束的說明、公開健康燈、`next` 只接受站內路徑）；S03 總覽（六個狀態磚、1 小時變化、24 小時上線數、系統健康條、進行中事件、群組健康、離線最久、資源使用前 10）；S04 主機清單（搜尋含 CIDR、狀態 chip、離線原因／群組／標籤／OS／版本、伺服器端排序與游標分頁、即時更新不重排）；S05 主機詳情（概覽、純 SVG 的趨勢圖〔平均＋最小／最大，空缺不補 0〕、24 小時時間軸、連線與憑證、連線紀錄）；全域搜尋（Ctrl+K）；S23 個人設定（主題、減少動態效果、時間顯示與時區〔只存在這個瀏覽器〕、變更密碼、兩步驟驗證〔本機產生 QR code、10 組復原碼只顯示一次〕、我的工作階段與「結束其他所有工作階段」）。沒有後端或沒有畫面的元素依 §28.6 隱藏（清單見 `kairos/web/README.md` 第 8 節）。
- **框架層**：路由、API 用戶端（CSRF 標頭、錯誤代碼 → 設計包的三段式說明、401 帶 `next` 回登入頁、403 顯示所需權限）、共用的 SSE 管理（重連、權限改變時重新判斷）、依 `/api/v1/me` 權限顯示的導覽與按鈕、亮暗主題、繁中文案集中（`strings.js`）、表格、對話框、Toast、空狀態／載入／錯誤狀態元件、瀏覽器端的閒置逾時。
- **API**（唯讀，`hosts:read`，群組範圍只含範圍內的主機）：`GET /api/v1/hosts/facets`（篩選列的選項與台數、群組的線上數）；`GET /api/v1/hosts/{agent_id}/connections`（Kairos 這次啟動後的上線與離線紀錄，記憶體中、每台最多 50 筆）；`GET /api/v1/hosts/{agent_id}` 新增 `certificate`（最新一張由 Kairos 簽發的憑證的狀態與到期，不含序號）。`kairos/API.md` 第 5.2、5.7、5.8 節。
- **安全標頭**：主控台的每個回應帶 HSTS、`X-Content-Type-Options`、`X-Frame-Options: DENY`、`Referrer-Policy: no-referrer`、COOP／CORP、`Permissions-Policy` 與 CSP（網頁：`script-src 'self'`、`style-src 'self'`、`frame-ancestors 'none'`、`require-trusted-types-for 'script'`；API：`default-src 'none'`）。
- **設計 token**：`kairos/web/assets/tokens.css` 為設計包 `design-system/tokens.css` 的複本，`cargo test` 檢查與記錄的 SHA-256 相同（找得到設計包時逐位元組比對）；`kairos/web/tools/sync-design.mjs` 重新複製並產生錯誤代碼的資料。
- **端對端測試**：`kairos/web/tests/run.sh`（Playwright＋axe，不在 `cargo test` 中）對真的 Kairos 執行流程測試（CSP、XSS、RBAC、兩步驟驗證、鍵盤、即時更新、深色主題、窄螢幕…；告警、註冊與憑證、管理各有獨立的測試檔），axe 0 違規。未指定的連接埠自動挑選空的連接埠，同一台機器可同時執行多份。

#### 變更（地基）

- **沒有出示任何憑據的請求不再扣驗證失敗的額度**（`kairos/API.md` 第 4.3 節）：沒有 `Authorization` 也沒有工作階段 cookie 的請求仍回 401，但不計入同一來源的失敗次數（未登入的瀏覽器開啟主控台時會讀取 `/api/v1/me`；否則重新整理登入頁幾次就會被限速）。出示錯誤或失效的 token、工作階段照常計入。
- `GET /`（以及其他不是 `/api/`、健康檢查的路徑）由 JSON 的 404 改為網頁介面（`/api/…` 不存在的路徑仍是 JSON 的 `not_found`）。

#### 告警畫面（S06～S11）

設計包 `kairos/screens.md` S06～S11、`alerts-ux.md`。只使用既有的告警 API（`kairos/API.md` 第 13 節），沒有新增端點。

- **S06 告警事件**：進行中／需要我處理／已恢復待結案／全部、嚴重度（附進行中數）／狀態／類型／規則／開始時間篩選與搜尋（寫在網址）、彙總事件展開子事件、即時更新（SSE `alert.*`，列就地更新、新事件提示「更新列表」）、行內與批次確認、指派、解決、從事件建立靜音；告警引擎沒有執行、暖機中、靜音生效、沒有通知管道的提示。
- **S07 事件詳情**：狀態列（嚴重度、處理類型、規則、負責人、升級、暫停）、判讀（由事件內容組出）、受影響主機（子事件）、處理紀錄與註解、通知發送狀態（含錯誤代碼與「查看錯誤」）；確認並指派給我（預計處理時間）、指派、暫停通知 1 小時、解決（根因必選）。
- **S08 告警規則**、**S09 規則編輯**：啟用開關（停用系統預設規則需要原因）、7 天觸發與雜訊；自訂規則（主機離線、長期失聯、回報延遲、CPU／記憶體門檻含恢復門檻、身分衝突）、範圍與符合台數、依嚴重度的通知預設、升級對象、歷史試算（離線類寫出不支援）。
- **S10 靜音與維護時段**：符合台數與範圍的文字、時長上限與伺服器驗證錯誤、大範圍與緊急等級的 L1 確認、提前結束。
- **S11 通知管道**：Email（SMTP）與 Webhook（Kairos JSON、Slack 類、Teams、LINE）、機密只顯示遮蔽值（只能更換）、寄送測試通知（含 429 限速）、`secrets_unavailable` 的處理方式、刪除（L2，替代管道）。
- **S03 總覽**：嚴重、警告磚改由進行中的告警推算（線上主機）；進行中事件可行內確認、連到事件詳情。側邊導覽的「告警事件」計數泡泡（緊急時實心）；全域搜尋加入告警事件。
- 隱藏（§28.6）：S06 群組與負責人篩選；S07「加入處理」、匯出 PDF、通知重送；S08／S09 未實作的規則類型與系統規則；S10 週期性維護與群組範圍；S11 每日彙整、HTML 版、Alertmanager 格式、代理伺服器。

#### 註冊與憑證畫面（S12～S15）

- **S12 註冊 token**：清單與部署進度、建立（核准方式、限定來源網段、批次部署、限定主機、簽發憑證有效期；自動核准限管理員）、建立完成只顯示一次（SecretField 預設遮蔽、CA 指紋、安裝字串、`pulse enroll --install-string` 指令；之後只顯示前 4 碼）、停用（L1）、詳情的部署進度漏斗；下載端點未提供，隱藏一行指令與 MSI 下載。
- **S13 待核准**：篩選與搜尋、全選只選無異常、批次核准（L1）、拒絕（L2：單筆輸入主機名稱、批次「拒絕 N 筆」，原因必填）、申請詳情（申請編號、公鑰指紋、旗標說明）、最近結束、每 10 秒確認清單變化。
- **S14 憑證**：狀態 KpiTile（含 CRL 剩餘、寬限期內、需重新註冊）、撤銷（L2）、詳情（`/certificates/:serial`、下載 PEM）、允許再次自動續簽（L1，限管理員；維運者停用並說明）、建立限定這些主機的 token、待簽發（匯出 CSR 佇列、匯入簽發結果）。
- **S15 CA 與撤銷清單**：每個簽發 CA 的 CRL（`KRS-E1203`、`KRS-W1201`）、立即更新 CRL（L1，維運者也可）、信任錨與簽發 CA（`KRS-W1304`）、簽發方式（`KRS-E1303`）、CRL 版本歷史。
- **API**：`GET /api/v1/pki/certificates` 與 `/{serial}` 另帶 `hostname`（主機清冊；尚未連線時取申請的主機名稱）與 `group`，`q` 也比對主機名稱片段（`kairos/API.md` 第 11.5、11.8 節）。
- 端對端測試另啟動一個 mTLS＋註冊端點＋線上簽發的 Kairos（`pki-env.sh` 以 openssl 建立測試 PKI），以實際的 `pulse enroll` 註冊、核准、取得憑證。

#### 管理與系統畫面（S16～S18、S20、S21）

- **S16 使用者與角色**：清單（來源、角色、狀態、兩步驟驗證、搜尋）、新增本機帳號（一次性密碼只顯示一次，關閉前須勾選「我已記下」）、詳情（有效權限、工作階段、擁有的 API token）、角色指派（L1，顯示權限差異）、停用／啟用、解除鎖定、重設密碼與兩步驟驗證、結束所有工作階段、刪除（L2）、角色與權限矩陣；自己與最後一位全站管理員的停用／刪除停用並說明。
- **S17 目錄服務**：連線設定唯讀（bind 密碼遮蔽、「此設定在 kairos.toml」）、未加密時的警告、以帳號測試登入、群組對應的新增與移除（L1）。
- **S18 稽核日誌**：查詢（條件在網址）、詳情（細節、前一筆雜湊、雜湊、HMAC）、驗證完整性（第一個斷點與原因、HMAC 邊界）、JSONL 匯出；維運者只看自己的操作（`audit:read_own`）。
- **S20 系統健康**：健康 API 各項，依權限加入簽發 CA 到期、註冊端點、告警引擎、`/metrics`；頂列的系統狀態燈改為連到這裡；「下載診斷包」改為可複製的 `kairos diag` 指令。
- **S21 系統設定**：一般、連線與安全、資料保留與儲存、整合；每個設定鍵的生效值、來源（環境變數／主控台／設定檔／內建預設）、修改者與時間；註冊端點模式與自動暫停（L1；關閉且有受影響主機時 L2）；改用設定檔的值；環境變數鎖定的鍵唯讀；API token（建立只顯示一次、清單前 4 碼、撤銷 L1）。維運者只有「連線與安全」（唯讀）。
- **伺服器端**：網頁介面路由第二段以後的參數可含「.」（`/admin/users/alice.w` 回 `index.html`；最後一段含「.」的其他路徑仍為 404）。

#### 導覽與共用（整合）

- **側邊導覽**依 `ia.md` §3：監控（總覽、主機、告警事件、告警規則、靜音與維護、通知管道）→ 註冊與憑證（待核准、註冊 token、憑證、CA 與撤銷清單）→ 稽核與報表（稽核日誌）→ 管理（使用者與角色、目錄服務）→ 系統（系統健康、系統設定）；項目依 `/api/v1/me` 的權限顯示，沒有可見項目的區塊不顯示標題。唯讀角色看到：總覽、主機、告警事件、靜音與維護、系統健康。
- **分頁標題的未確認數**：有未確認的告警時為「(3) Kairos｜總覽」，與「告警事件」的計數泡泡相同（`ia.md` §7）。
- **無障礙（WCAG A 級，設計包 §29.5 #6、#7）**：告警事件的 `j`／`k` 快速鍵只在焦點位於事件表格內時生效（2.1.4，不再整頁攔截）；閒置逾時前 2 分鐘以對話框提醒（`alertdialog`，焦點在「繼續使用」、`aria-live` 讀出剩餘時間），「繼續使用」或 Esc 延長，逾時才登出（2.2.1）。
- **稽核日誌的斷點**（S18）：同為 `KRS-E6201`，金鑰問題（`hmac_key_unavailable`、`hmac_key_mismatch`）另以「無法驗證 HMAC」與「從備份還原 `audit-hmac.key`」呈現（設計包範例 95）；`kairos audit` 說明的結束代碼 9 改為「稽核日誌完整性驗證失敗（雜湊鏈斷裂、HMAC 不符或無法驗證 HMAC）」。
- **未儲存的變更**：規則編輯（S09）、通知管道（S11）、系統設定的註冊端點模式（S21）有未儲存的變更時，站內換頁（連結、上一頁／下一頁）先詢問「要放棄變更嗎？」，關閉或重新整理分頁時由瀏覽器詢問。

#### 已知限制

- **沒有畫面**：S02 首次啟動精靈、S19 合規報表、S22 授權資訊（選單不顯示）；主機的寫入操作（編輯、維護、除役、刪除資料）與主機詳情的資產資訊、事件、稽核紀錄分頁。
- **依 `decisions/INTEGRATION.md` §28.6 隱藏的元素**（沒有後端；各畫面的完整清單見 `kairos/web/README.md` 第 8 節）：S06 群組與負責人篩選；S07「加入處理」、匯出 PDF、通知重送；S08／S09「憑證即將到期」「版本過舊」類型與未實作的系統規則；S10 週期性維護與群組範圍；S11 每日彙整、HTML 版、Alertmanager 格式、代理伺服器；S16「從目錄服務加入」與權限清冊 CSV；S17 定期同步與連線設定的主控台編輯（唯讀）；S18 CSV 匯出、外送 SIEM、保留期限的調整；S20「下載診斷包」（改為 `kairos diag` 指令）與目錄服務列；S21「主機群組與標籤」分頁、`[log]`／`[observability]` 與需要重新啟動的設定唯讀；S23 我的通知、我的 API token。
- **註冊與憑證**（web-pki 線回報）：沒有安裝程式下載端點，只提供 `pulse enroll --install-string`（INTEGRATION §27.4 #3）；S13 沒有排隊台數與佇列已滿提示、OS 與來源網段篩選；S14 沒有匯出清冊、手動簽發 CSR、重新簽發、憑證政策的修改；S15 沒有上傳／下載 CRL、CA 輪替精靈（簽發 CA 以 CLI 管理）。
- **導覽**：只有「告警事件」有計數泡泡；「待核准」與「系統健康」的計數泡泡（`ia.md` §7）、全域橫幅（CRL 到期等）尚未提供。S03 總覽的範圍選擇器、憑證磚與最近變更尚未提供。
- **未儲存的變更**：上一頁／下一頁取消離開時以 `pushState` 放回原網址，瀏覽器的「下一頁」紀錄會被清除。
- 連線紀錄只在記憶體中（重新啟動後從頭記錄）；完整、持久的連線紀錄屬於之後的版本。
- 偏好（主題、時區）沒有伺服器端儲存，只套用在各自的瀏覽器。
- **支援的瀏覽器：Chromium 系（Chrome、Edge）**（設計包 D44 #2）。Trusted Types 只有 Chromium 支援；Firefox、Safari 可以使用（CSP 的其餘部分照常生效），但未在發佈前驗證，跨瀏覽器與真人螢幕報讀器的驗證屬於之後的版本。

## [0.6.0] - 未單獨發佈（內容隨 0.8.0 發佈）

**Kairos 0.6.0、Pulse 0.6.0**：衝刺 5 的三條線——**告警引擎**（審計 F3；設計包 `kairos/alerts-ux.md`、`screens.md` S06～S11、§24 #2、#8、#9）、**使用者、角色、工作階段與稽核**（審計 F6、S7 的雜湊鏈部分；`screens.md` S01、S16～S18、S23、§24 #10、#11、D25）、**可觀測性**（審計 O3、O8；`screens.md` S20、§24 #8、`pulse/log-ux.md`、`pulse/cli-spec.md` §6、§8）。**協定不變**（v1～v2，8443 的 feature 與 0.5 相同），Kairos 與 Pulse 0.5／0.6 的任何組合都能照常連線。Pulse 只有可觀測性的變更。

規格：告警 [`kairos/ALERTS.md`](kairos/ALERTS.md)、`kairos/API.md` 第 13 節；使用者與稽核 `kairos/API.md` 第 12 節；健康檢查與 `/metrics` 見 `kairos/README.md`「健康檢查與 Prometheus」；資料表 `kairos/STORAGE.md` 第 3 節。

### 破壞性變更與升級步驟（升級前必讀）

| # | 產品 | 變更 | 誰會受影響 | 升級步驟 |
|---|---|---|---|---|
| 1 | Kairos | **資料庫升級到 schema v6**（0.5.0 為 v4；依序套用 v5、v6，各自一個交易）。v5：告警的 `alert_rules`、`alert_events`、`alert_timeline`、`alert_silences`、`alert_channels`、`alert_deliveries`、`alert_actions`；v6：`users`、`user_recovery_codes`、`role_assignments`、`sessions`，`api_tokens` 加 `owner_user_id`、`role`，`audit_events` 加 `prev_hash`、`hash`，`hosts` 加 `host_group` | 所有 SQLite 部署 | 升級前 `kairos db backup`；升級後 0.5 無法開啟資料庫（降版從備份還原）。`kairos alerts`、`users`、`audit`、`api-token` 等開啟資料庫時同樣會升級 schema |
| 2 | Kairos | **資料目錄多了兩個金鑰檔**：`alerts-secrets.key`（通知管道機密的 AES-256-GCM 金鑰）、`console.key`（CSRF 與 TOTP 秘密），都是 0600，`kairos db backup` **不含** | 備份、還原、搬移資料目錄的部署 | 與資料庫備份一起（加密）保存，**還原時一起複製**（INSTALL 第 10.2 節、STORAGE.md 第 9 節）；少了 `alerts-secrets.key` 時管道的機密需重新輸入，少了 `console.key` 時 TOTP 需重設 |
| 3 | Kairos | **0.4／0.5 的 API token 權限凍結在升級前**：`read`／`operate`／`admin` 範圍不會得到 0.6 新增的權限（使用者、稽核、目錄服務、API token 管理、**告警**）。`kairos api-token create` 的預設改為 `--role viewer`；`--scope` 已棄用（仍可使用，權限同樣凍結） | 要讀寫告警、使用者或稽核的整合；以 `api-token create` 不帶參數建立 token、並讀取 `/api/v1/enroll`、`/pki`、`/settings` 的腳本 | 以 `--role viewer`（告警與靜音唯讀）、`--role auditor`（唯讀全部）、`--role operator`（處理告警、核准註冊）或 `--role admin` 建立新 token。既有 token 的既有用途不受影響 |
| 4 | Kairos | **告警引擎預設啟用**（`[alerts] enabled = true`），第一次啟動時建立 30 條系統預設規則；**沒有任何通知管道**，事件只出現在主控台與 `kairos alerts list` | 所有 SQLite 部署 | 不需要時 `[alerts] enabled = false`。需要通知時由管理員新增管道（`POST /api/v1/channels`）並綁到規則 |
| 5 | 兩者 | **日誌行首的時間改為 RFC 3339、毫秒、帶時區位移**：`2026-10-14 14:22:16 [INFO] …` → `2026-10-14T14:22:16.123+08:00 [INFO] …`；有固定事件的行在訊息前加「`<event> <code> `」（審計 O3） | 以 grok、正規表示式或 `cut` 解析行首時間的收集規則、腳本 | 更新規則；需要時間過渡時設定 `log.format = "legacy"`（與 0.5 相同，保留一個主要版本）。建議改用 `log.format = "json"` |
| 6 | Kairos | **主控台的監聽上新增不需要認證的 `/healthz`、`/readyz`**（只回傳狀態字），以及 `/metrics`（未設定 `observability.metrics_token_file` 時只接受 loopback） | 主控台開放給管理網段、且不希望任何未認證端點的部署 | 設定 `observability.on_console = false`（端點改由 `observability.listen` 提供，或完全不提供） |
| 7 | Kairos | 主控台 API 的 403 多了 `details.permission`；權限不足的寫入請求寫入稽核（`access.denied`）。主機 API 的 `offline_reason` 多了 `write_unknown` | 解析 403、依 `offline_reason` 判斷的整合 | 只新增欄位與值；讀取端應忽略不認得的值 |

**升級步驟**：先 `kairos db backup` 並另外保存資料目錄的金鑰檔 → 換上 Kairos 0.6.0（`kairos --check-config` 確認設定）→ 以 `kairos users create <帳號> --role admin` 建立首位管理員（沒有預設帳號與密碼；INSTALL 第 9.4 節）→ 需要通知時新增管道並綁到規則 → 以 `--role` 重新建立需要告警權限的整合 token。Pulse 可以任何順序升級（只有日誌格式與本機指標）。

### Kairos：告警

- **內部事件匯流排**（`kairos/src/bus.rs`）：狀態表（上線、離線含原因分類、被取代）、連線（身分驗證失敗、狀態表淘汰、自行除役）、TLS 重載、系統檢查的狀態變化以結構化事件發布；告警引擎與 `/metrics` 都訂閱它。斷線原因：`idle_timeout`、`agent_closed`、`certificate_rejected`、`server_shutdown`、`write_unknown`。
- **告警引擎**（`kairos/src/alerts/`）：四級嚴重度（D3）與處理類型；準位觸發、去重、10 分鐘內重新開啟、抖動偵測；觸發 → 確認 → 恢復 → 解決（未確認 24 小時、已確認 7 天自動結案，緊急不自動結案）；關聯彙總（系統事件 → 網段／大規模離線／重新啟動後未恢復 → 主機事件）；暖機期與停機事件；30 條系統預設規則與使用者規則（主機離線、長期失聯、回報延遲、CPU／記憶體門檻、身分衝突）；靜音與維護時段（D23、D26，伺服器端驗證上限）；規則的歷史試算。
- **通知**：Email（SMTP，STARTTLS／TLS、AUTH PLAIN／LOGIN、私有 CA）與 Webhook（`kairos.alert.v1`＋HMAC-SHA256 簽章、Slack 類、Teams、LINE）；重送與升級、每管道 30 秒合併、緊急不排隊、重試與退避、15 分鐘放棄、連續 3 次失敗 `KRS-E5107` 並改經其他管道；機密以 AES-256-GCM 加密保存，API、CLI、日誌、稽核、診斷包只有遮蔽值。hyper 啟用 `client` feature（帶入 `want`、`try-lock`）。
- **主控台 API**（`/api/v1/alerts`、`/alert-rules`、`/silences`、`/channels`，API.md 第 13 節）與 SSE 的 `alert.created`／`alert.updated`／`alert.resolved`；**CLI** `kairos alerts|silences|alert-rules|alert-channels`；**設定** `[alerts]`。

### Kairos：使用者、角色、工作階段與稽核

- **本機帳號**：Argon2id（m=19 MiB、t=2、p=1，可調）、密碼政策、連續失敗 5 次鎖定 15 分鐘（`KRS-E6003`；不存在的帳號回應相同）、依來源 IP 限速、第一次登入必須變更密碼。**TOTP** 與 10 組復原碼。**LDAP／AD**（LDAPS／StartTLS，一律驗證伺服器憑證；群組對應角色）。依賴 `argon2` 0.5、`ldap3` 0.12（`tls-rustls-ring`）。
- **RBAC**（API.md 第 12.1 節）：權限集中在 `kairos/src/rbac.rs`（35 個，含告警的 10 個），四個內建角色（管理員、維運者、稽核員、唯讀，依 `ia.md` §4.2），範圍為全站或主機群組。告警的權限（`alerts:read|write`、`alert-rules:read|write|admin`、`silences:read|write`、`channels:read|test|write`）：管理員全部；維運者處理事件、自訂規則、靜音、管道清單與測試；稽核員事件／規則／靜音唯讀；唯讀事件與靜音唯讀；**管道的新增、修改、刪除（含機密）與系統規則的條件只限管理員**；群組範圍的指派不授予告警的權限（API.md 第 13.1 節）。
- **工作階段**：`__Host-kairos_session` cookie（HttpOnly、Secure、SameSite=Strict）、CSRF token（`X-CSRF-Token`＋Origin／`Sec-Fetch-Site` 檢查，告警的寫入同樣需要）、閒置 30 分鐘與絕對 12 小時逾時；D25 的兩種結束 API。
- **使用者管理 API 與 CLI**（`kairos users …`）、**API token 屬於角色或使用者**（`--role`、`--user`）。
- **稽核雜湊鏈**（API.md 第 12.8 節）：每筆 `audit_events` 含前一筆的雜湊；登入、權限變更、token、設定、註冊與 PKI、被拒絕的請求、**告警的確認、指派、解決、註解，靜音、規則、管道的變更**（API 與 CLI）、系統啟動與關機都在鏈中；`kairos audit list|verify|export`、`POST /api/v1/audit/verify`（`KRS-E6201`）。
- SSE（`/api/v1/events`）在每次心跳重新計算權限：撤銷、工作階段結束、停用或降權後不再推送；`alert.*` 只送給全站範圍有 `alerts:read` 的身分。

### Kairos：可觀測性

- **`/healthz`、`/readyz`、`/metrics`**：主控台的 HTTPS 監聽上提供（`observability.on_console`），另可以 `observability.listen` 純 HTTP 監聽。`/readyz` 不取得寫入鎖檢查資料庫。
- `/metrics`：連線、狀態表、被拒的連線、report 寫入與 `ack` 延遲直方圖、資料庫與保留、TLS 與憑證剩餘天數、註冊端點、主控台 API（`route` 含 0.6 的新路徑）、日誌與行程，以及**告警**：引擎狀態（`kairos_alerts_engine_up` 等）、未解決事件（`kairos_alerts_open{severity,state}`）、建立與解決的事件數、管道狀態、通知的發送結果（`kairos_alert_notification_sends_total{kind,result}`）與最後狀態（`kairos_alert_notifications_total{kind,state}`）。**主機的上線、離線、被取代、hello 身分驗證失敗（新 `kairos_agent_auth_failures_total`）與狀態表淘汰改由事件匯流排訂閱計數**；`kairos_agent_disconnects_total` 的 `reason` 與 Webhook 的 `offline_reason` 相同（`idle_timeout`、`agent_closed`、`server_shutdown`、`certificate_rejected`、`write_unknown`；主機 API 仍為 0.4 起的 `timeout`、`connection_closed`）（與告警引擎同一個來源，不重複計數，`connection.rs` 不變）。標籤只用有限集合的值，不以 agent_id 或 IP 當標籤。
- **JSON 日誌與 syslog**（`log.format = text|json|legacy`、`log.timezone`、`log.stdout`、`[log.syslog]`）；告警與帳號模組的日誌有固定的 `event`（例 `alerts.channel.failed`、`alerts.engine.started`、`audit.chain_broken`、`login.directory_failed`）。
- **`kairos --check-config`**、**`kairos diag`**（支援診斷包，0600）。診斷包一律遮蔽私鑰、API token、註冊 token、安裝字串、Bearer、名稱含 token／secret／password 的值，**另遮蔽密碼雜湊（PHC）、TOTP 的 `otpauth://`、工作階段 cookie 與 CSRF token、網址中的秘密（帳號密碼、查詢字串、Webhook 路徑中的長片段）**；`alerts-secrets.key`、`console.key`、資料庫與私鑰檔的內容永不收集。

### Pulse

- **本機指標**（`[metrics]`，預設關閉）：loopback 的 HTTP `/metrics` 或 textfile collector 的檔案。
- **JSON 日誌與 syslog**（同 Kairos；多實例時 JSON 的 `instance` 為實例編號 N，取自 `pulse-N.service` 的 `StateDirectory=pulse/N`，單一實例為 `null`）、**`pulse check-config`**（`log.format = "legacy"` 時警告 `PLS-W0104`，與 Kairos 的 `KRS-W0104` 同號）、**`pulse diag`**（遮蔽規則與 Kairos 共用）。
- `pulse.service` 維持 `StandardOutput=null`（理由見 unit 與 INSTALL.md 7.1）。

### 共用

- 新的內部 crate `crates/kp-obs`：日誌格式、syslog（RFC 5424）、Prometheus 文字格式與測試用的嚴格解析器、診斷包（tar.gz）與遮蔽。只用 workspace 既有的依賴。
- rustls 0.23.45 → 0.23.46（RUSTSEC-2026-0346～0349，`cargo deny` 的新公告）。

### 新增的產品代碼（設計包 `decisions/INTEGRATION.md` §28.3 正式分配）

| 代碼 | 意義 | 使用處 |
|---|---|---|
| `KRS-E6012` | 兩步驟驗證碼不正確或已使用過（HTTP 401 `invalid_mfa_code`） | 登入第二步 |
| `KRS-E6013` | 第一次登入必須先變更密碼（HTTP 403 `password_change_required`） | 主控台 API |
| `KRS-E6105` | 工作階段的寫入請求缺少或帶錯 CSRF token、或來自其他網站（HTTP 403 `csrf_failed`） | 主控台 API |
| `KRS-E5109` | SMTP 伺服器拒絕寄件者、收件者或內容 | 通知 |
| `KRS-E5110` | 通知管道的設定無效（HTTP 400 `channel_invalid`） | 告警 API |
| `KRS-E0101` | 設定檔語法或型別錯誤（類比 `PLS-E0101`） | `kairos --check-config` |
| `KRS-E0401` | `--config` 指定的設定檔不存在（類比 `PLS-E0401`） | `kairos --check-config` |
| `KRS-E0402` | 設定檔無法讀取：權限、I/O（類比 `PLS-E0402`） | `kairos --check-config` |
| `KRS-W4302` | 診斷包內容已截斷（類比 `PLS-W4302`） | `kairos diag` |

`kairos diag` 無法產生時沿用 `KRS-E4301`（§28.3 定案為語意延伸：「無法產生匯出檔案或診斷包」）。工作階段結束或逾時沿用 `KRS-E6010`（`details.reason = "session_expired"`，§28.3 定案）。啟動時的設定檔錯誤仍只印出訊息與結束代碼 3（代碼只在 `--check-config` 中顯示）。

### 量測

可觀測性線在 kp-build（4 vCPU）以 `api_bench_acks_under_load`（每秒 2,000 筆 report、1 萬台主機）量測 `ack` p99：只有寫入 2.21 ms、4 個 API 用戶端連續查詢 5.81／5.40 ms、每秒抓取一次 `/metrics` 5.43／0.97 ms、連續抓取 `/metrics` 4.06／1.72 ms（0.5.0：2.43／1.92、14.26／4.89 ms）。整合後 `/metrics` 的主機計數改為事件匯流排的訂閱者（不在連線的程式中），告警引擎在自己的執行緒上。

### 已知限制

- 告警的權限只由全站範圍的角色指派授予；群組範圍的維運者看不到告警（群組內的告警處理延後）。
- 週期性維護時段、警告的每日彙整、個人勿擾與個人通知、部分系統規則（資料磁碟空間、CRL 自動更新失敗、agent 憑證到期提醒等）、主機離線類規則的歷史試算尚未實作（ALERTS.md 第 10 節）。
- 目錄服務的定期同步、「從目錄服務加入」、群組範圍的寫入權限、主機群組的管理 API、雙人覆核、稽核外送延後（API.md 第 12.11 節）。
- 日誌的 `event`：其餘模組尚未指定事件的行以模組路徑代替；`text` 版面、`log-ux.md` §5 的重試摘要等尚未實作。`/metrics` 的計數為啟動以來的累計值。syslog 的 `unix:///dev/log` 送出 RFC 5424（journald 請改用 stdout）。`pulse check-config --connect`、`--show-effective`、`pulse diag --include-debug-log` 尚未實作。

### 測試

- 端對端（`tests/e2e/tests/alerts.rs`，日誌為 JSON）：首位管理員（CLI）→ 登入並變更密碼 → 建立維運者與唯讀 → 管理員新增 Webhook 管道（測試內的本機 HTTP 接收器）並綁到主機離線規則 → Pulse 停止 → Webhook 收到簽章的 `kairos.alert.v1` → 唯讀確認 403、沒有 CSRF token 403 → 維運者確認 → `/metrics` 有告警、通知與主機的計數 → `kairos audit verify` 通過，`alert.ack`、`channel.create`、`access.denied` 在鏈中；Kairos 與 Pulse 的每一行輸出都是 v1 的 JSON 日誌。另有 `users.rs`（首位管理員與稽核鏈）。
- Kairos：RBAC 矩陣測試含所有告警端點（四個角色＋沒有角色）與工作階段寫入的 CSRF 拒絕；告警的權限表與設計矩陣一致；0.5（v4）資料庫升級到 v6 後三種舊 token 的權限不變、告警端點一律 403、SSE 收不到 `alert.*`；告警操作的稽核在雜湊鏈中；`/metrics` 的告警與匯流排計數；JSON 日誌欄位；診斷包不含金鑰檔、密碼雜湊、TOTP、工作階段、CSRF、SMTP 密碼與 Webhook 網址的秘密。
- `kp-obs`：Prometheus、JSON／text、syslog、tar／gzip、遮蔽（含新的憑證類別與冪等）。Pulse：本機指標、`check-config`、`diag`。

### 發佈與建置

- Kairos 發佈包加入 `prometheus.example.yml` 與 `kairos-alerts.example.yml`；Pulse 發佈包加入 `pulse-alerts.example.yml`。
- 新依賴：`argon2`、`ldap3`（及其依賴），`cargo deny check` 通過；新的內部 crate `kp-obs`（publish = false）。

## [0.5.0] - 未單獨發佈（內容隨 0.8.0 發佈）

**Kairos 0.5.0、Pulse 0.5.0**：衝刺 4「註冊通道 8444」。Kairos 新增註冊通道（8444 HTTPS）、自動簽發的 PKI（線上簽發 CA、CRL）與管理 API／CLI；Pulse 新增 `pulse enroll`、`pulse decommission`、過期寬限期續簽與 8443 的自動輪替。**協定版本不變**（仍為 v1～v2），8443 以新的 feature `renew`、`deregister` 協商，只在雙方都宣告時使用：0.4 的 Kairos 與 Pulse 不受影響，兩個產品可以分批升級（建議順序見下）。規格：`crates/kairos-proto/ENROLL.md`（8444）、`crates/kairos-proto/PROTOCOL.md`「憑證輪替與除役」（8443）、`kairos/PKI.md`、`kairos/API.md` 第 11 節。

### 破壞性變更與升級步驟（升級前必讀）

| # | 產品 | 變更 | 誰會受影響 | 升級步驟 |
|---|---|---|---|---|
| 1 | Kairos | **資料庫升級到 schema v4**：新增 `pki_cas`、`enroll_tokens`、`enroll_requests`、`pki_certificates`、`pki_crls`、`enroll_replay`、`enroll_blocks`、`enroll_relaxations`、`settings`、`audit_events`，`hosts` 新增憑證與除役的欄位 | 所有 SQLite 部署 | 升級前 `kairos db backup`（Kairos INSTALL 第 11 節）；升級後 0.4 無法開啟資料庫（降版從備份還原）。`kairos enroll-token`、`kairos enroll`、`kairos pki`、`kairos config` 開啟資料庫時同樣會升級 schema |
| 2 | Kairos | **API token 新增 `operate`、`admin` 範圍**（階層式） | 以主控台 API 執行寫入的整合 | 既有 token 維持 `read`，寫入操作回 403（`details.required`）；需要核准、撤銷、模式切換時以 `kairos api-token create --scope operate|admin` 建立新的 token |
| 3 | Kairos | **主控台修改的設定存在資料庫、優先於 `kairos.toml`**：`enroll.mode`、`enroll.auto_pause_*`、`pki.policy.*`（環境變數仍最優先） | 以 `kairos.toml` 管理這些鍵的部署 | 以 `kairos config show --effective` 查看每項的實際值與來源；`kairos config reset <鍵>` 改回設定檔的值。啟動時以 `INFO` 列出被覆寫的設定檔設定 |
| 4 | Kairos | 啟用註冊通道時，**檔案描述元的需要量另加 `enroll.max_connections`**（預設 1024） | 自訂 `LimitNOFILE` 的部署 | 啟動時不足會記 `WARN` 並附建議值；發佈包 unit 的 `LimitNOFILE=8192` 足夠 |
| 5 | Pulse | **要使用 `pulse enroll` 時需要新版 `pulse.service`**（`ReadWritePaths=` 多了 `-/opt/pulse/identity`）與 `identity/` 目錄 | 以 `pulse enroll` 註冊的主機 | `install -d -o pulse -g pulse -m 700 /opt/pulse/identity`，換上新版 unit 並 `systemctl daemon-reload`（多實例時重新產生每個 `pulse-N.service`，Pulse INSTALL 第 9、10 節）。沿用手動憑證的部署不需要任何變更：沒有 `identity/enrollment.json` 時行為與 0.4 相同 |

**沒有 `[enroll]`、`[pki]` 的 Kairos 與沒有 `identity/` 的 Pulse，行為與 0.4 相同**：註冊通道關閉（不監聽 8444，啟動時記一次 `KRS-I2010`）、離線簽發，手動 PKI（`auth.client_ca`、`auth.crl`）照舊。

**升級步驟**（建議依序；只升級其中一個產品也可以，見「相容性」）：

1. **Kairos 升級前**：`kairos db backup` 備份資料庫（第 1 項）。
2. **升級 Kairos**：換上新版執行檔並重新啟動（Kairos INSTALL 第 11 節，unit 不需修改）。此時註冊通道仍關閉，0.4 的 Pulse 照常以 8443 連線（`hello_ack` 與 0.4 相同）。
3. **（要使用自動註冊時）建立簽發 CA**：`kairos pki issuing-ca new` → 以代理 CA（或根 CA）簽發 → `kairos pki issuing-ca install`（Kairos INSTALL 第 6.5 節）；在 `kairos.toml` 加上 `[enroll]`（`mode`）與 `[pki]`（`mode = "online"`、`server_ca_path`）並重新啟動，確認日誌有「註冊端點（8444）已啟動」。
4. **開放 8444 的防火牆**：只允許**要註冊的主機**連到 Kairos 的 TCP 8444（或 `enroll.listen` 的連接埠）。不使用自動註冊時不必開放。
5. **升級 Pulse**：換上新版執行檔與 `pulse.service`（第 5 項），`install -d -o pulse -g pulse -m 700 /opt/pulse/identity`，`systemctl daemon-reload` 後重新啟動（Pulse INSTALL 第 10 節）。既有主機沿用手動憑證即可；新主機以 `pulse enroll --install-string …` 註冊（Pulse INSTALL 第 5.0 節）。
6. **確認**：`pulse status`（連線列的 features 出現 `renew` 表示身分由 `pulse enroll` 管理且 Kairos 支援自動輪替）、`kairos enroll list`、`kairos pki certificates list`。

### 協定（kairos-proto）

- `crates/kairos-proto/ENROLL.md`：Pulse ↔ Kairos 註冊通道（8444 HTTPS）協定 v1 的唯一規格——端點 `/v1/enroll/server-info`、`token-check`、`requests`（新註冊）、`requests/{request_id}`（輪詢與取回憑證，含長輪詢）、`renewals`（過期寬限期續簽）、撤回的兩段式 challenge／withdraw；錯誤碼與 `KRS-`／`PLS-` 對照、時鐘檢查、三段模式、多層速率限制、同一個 CSR 只簽發一張憑證（設計包 `INTEGRATION.md` §23.9 定案）、安全審查項目的處置、端對端測試情境與實作現況（第 19.5 節）。
- `kairos-proto` 的 `enroll` 模組：註冊通道的請求與回應型別、欄位驗證、路徑與模式白名單、註冊 token（`KRT1-…`，GF(32) 檢查碼）、安裝字串、CA 指紋、申請編號、簽章內容的正規化與 ECDSA P-256 驗證；單元測試直接解析 ENROLL.md 的錯誤碼表、範例與測試向量。`kairos-proto` 因此依賴 `ring`、`chrono`（兩者原本就在依賴樹中）；測試另用 `rcgen`、`x509-parser`（MIT OR Apache-2.0）。
- **8443 的 feature `renew`、`deregister`**（ENROLL.md 第 13 節的草稿移入 PROTOCOL.md「憑證輪替與除役」）：`kairos-proto` 新增 `lifecycle` 模組與 `ClientMsg::RenewRequest`／`Deregister`、`ServerMsg::RenewResult`／`DeregisterAck` 變體，`hello` 的 `intent`、`hello_ack` 的 `expired_renew_grace_days`／`renew_now`（型別不符時視為沒有，`None` 時不輸出），錯誤碼 `renew_requires_approval`、`invalid_csr`、`rate_limited`（非 fatal）。兩個產品都以這些共用型別收發（合併前各自定義的 `pulse/src/lifecycle.rs` 與 Kairos 的自行解析已刪除）。PROTOCOL.md 新增輪替、除役與三個錯誤碼的測試向量，並以凍結的舊版型別驗證舊端看到新訊息時的行為（舊 Kairos 回 `unknown_type`、舊 Pulse 忽略多出的欄位）。

### Kairos

- 註冊通道 8444（`crates/kairos-proto/ENROLL.md`）：`server-info`、`token-check`、新註冊、輪詢（含長輪詢）、過期續簽、撤回（challenge／withdraw）；三段模式 `open`／`renew_only`／`closed` 與 24 小時沒有有效 token 的自動暫停；時鐘偏差檢查；同一個 CSR 只簽發一張憑證；重送防護（跨重啟）。
- 多層速率限制 L1～L10（連線、來源、token、agent、全域、待核准佇列、輪詢間隔），`429`／`503` 附依佇列位置計算的 `Retry-After`；無效 token 的來源封鎖（再犯加倍，寫入資料庫）；批次部署來源的倍率。8444 在獨立的 runtime 上執行，1 萬台規模下 report 的 `ack` p99 維持在數十毫秒（PKI.md 第 12 節）。
- PKI（`kairos/PKI.md`）：線上簽發 CA（`data/pki/`，私鑰 0600；權限或成對檢查不通過時不使用，`KRS-E1303`）、離線簽發佇列（`kairos pki queue export`／`sign-bundle`／`queue import`，不通過時 `KRS-E1305`）、簽發 CA 的建立／安裝／啟用／退役與到期預警（`KRS-W1304`，預設剩 455 天）；依政策簽發用戶端憑證；Kairos 自行產生簽發 CA 的 CRL（每 `pki.crl_refresh_hours`＝24 小時與每次撤銷）。
- 8443：信任錨加入簽發 CA 與其 CRL（依簽發者選擇驗證器），撤銷後立即重新載入；`renew`（到期前輪替、`hello_ack.renew_now`）、`deregister`（主機自行除役，撤銷憑證，之後 10 分鐘內只接受除役）；新憑證第一次連線後以 `superseded` 撤銷舊憑證。`renew`、`deregister` 只在 client 宣告時才列進 `hello_ack.features`；沒有協商時 `renew_request`／`deregister` 照舊回 `unknown_type`（線上格式與 0.4 相同）。
- 主控台 API 第 11 節：註冊 token、申請（含批次核准）、模式與影響評估、憑證政策、憑證清冊與撤銷、CRL、簽發 CA、離線簽發佇列、`GET /api/v1/settings`（每項的來源、修改者、被覆寫的值）與 `POST /api/v1/settings/reset`（改用設定檔的值）；下載端點不可用時以 `downloads` 標示。token 只顯示前 4 碼（`KRT1-XXXX…`）。所有寫入記稽核（`audit_events`）。
- CLI：`kairos enroll-token`、`kairos enroll`（list／show／approve／reject／mode）、`kairos pki`（issuing-ca list／new／install／activate／retire、certificates list、revoke、queue export／import、sign-bundle）、`kairos config show --effective`、`kairos config reset`；`kairos api-token create --scope read|operate|admin`。
- 設定：`[enroll]`、`[pki]`、`[pki.policy]`（`kairos.example.toml`）；8444 與 8443／9443 的監聽衝突在啟動時拒絕（`KRS-E0202`）；檔案描述元的需要量包含 `enroll.max_connections`。新的產品代碼 `KRS-E1303`、`KRS-W1304`、`KRS-E1305`。
- 修正：`kairos api-token list` 的「最後使用」超過 7 天改顯示日期時間（copywriting §5.3）；`kairos enroll-token create` 的安裝字串在註冊端點不是 8444、又沒有設定 `enroll.advertise` 時漏了 `e`（Pulse 因此連到 8444），改為與主控台 API 相同；8443 的 `renew`、`deregister` 只在連線以用戶端憑證證明了 `agent_id` 時才協商（合併前 `renew` 沒有檢查：未啟用 mTLS 而註冊服務開啟時，任何連線都能以自行宣告的 `agent_id` 取得簽發的憑證）。

### Pulse

- **`pulse enroll`**（設計包 `cli-spec.md` §5）：以安裝字串（`pulse-enroll:v1:…`）或 `--server`／`--token`／`--ca-pin` 向 Kairos 註冊。私鑰（ECDSA P-256）在本機產生、不離開主機；只信任 CA 指紋、`--ca-file` 或人工確認的 CA（TOFU 需 `--trust-on-first-use` 或互動確認，`PLS-W1407`），不使用作業系統信任庫，根 CA 可為 P-256、P-384 或 RSA；需人工核准時顯示申請編號並（長）輪詢；`--status`、`--cancel`（撤回申請）、`--force`、`--reset-identity`、`--json`（`pulse.enroll/v1`）。中斷後重新執行從 `identity/enrollment.json` 接續，回應遺失時以同一個 CSR 與輪詢憑據重送（不會產生第二筆申請）。
- **服務的註冊閘門與背景輪詢**：沒有可用身分時先完成進行中的申請（依 `next_poll_at` 恢復）；申請被拒、逾期、token 無效等無法自動恢復時不連線並以嚴重呈現。429／503 依 `Retry-After` 排隊（`PLS-E2202`／`PLS-E3309`），Kairos 因時鐘偏差暫不受理時每 5 分鐘重送並在本機時間被調整時立即重送（`PLS-E8302`）。
- **過期寬限期續簽**：Kairos 以 `CertificateExpired` 拒絕用戶端憑證時，經 8444 以舊私鑰對含 `sent_at` 的內容簽章自動續簽；`grace_blocked`（`PLS-E2007`，並對 8443 報到）、`grace_pending`、`grace_exceeded`（`PLS-E1113`）、`grace_rejected`（`PLS-E1114`）、`grace_clock_rejected`（`PLS-E8302`）；`replayed_request` 重新簽章重送一次。
- **8443 正常輪替與除役（feature `renew`、`deregister`）**：身分由 `pulse enroll` 管理時 hello 宣告 `renew`，到期前（已用 2/3 或剩餘 `identity.renew_before_days` 天）送出新的 CSR，以新憑證連線成功後才取代；Kairos 不支援時維持 0.4 的行為。`pulse decommission --initiated-by …`（安裝程式完全移除時）：已註冊經 8443 除役通知（hello 帶 `intent: "deregister"`、只宣告 `deregister`）、等待核准中經 8444 撤回（`PLS-W3107`／`PLS-W3310`）。手動憑證的主機不宣告這兩個 feature，hello 與 0.4 相同。
- **`pulse status`**：「連線」「身分」列與 `--json` 的 `identity.state`、`identity.enrollment`、`identity.grace`、`identity.renew_after`、`clock.source`（`clock_skew`、`server_info_unverified`）、`connection.features` 列出協商到的 `renew`；`--brief` 的 `pending_approval`、`clock_rejected offset=…`、`cert_expired grace=… renew=…`。
- 設定：`[identity]`（`dir`、`renew_before_days`）、`[enroll]`（`address`、`port`）。日誌事件 `enroll.*`、`cert.renewed`、`cert.renew_*`、`cert.grace_*`、`install.*_unreported`；`ERROR-CODES.md` 新增 30 個代碼的對應。
- 修正（整合時以端對端測試發現）：過期續簽第一次送出沒有成功（回應遺失、429、時鐘偏差、網路錯誤）時，服務誤判為「申請已被 `pulse enroll` 改變」而停止續簽；註冊端點位址其實是別的 HTTPS 服務（`not_found`）時改為 `PLS-E2106`（原為 `PLS-W2107`）。

### 相容性

| Kairos ＼ Pulse | Pulse 0.4（以前） | Pulse 0.5，手動憑證 | Pulse 0.5，`pulse enroll` |
|---|---|---|---|
| **Kairos 0.4（以前）** | 與 0.4 相同 | 與 0.4 相同（hello 不宣告 `renew`／`deregister`） | 無法註冊：8444 沒有監聽 → `PLS-E2007`；該位址是別的服務 → `PLS-E2106`（結束代碼 5）。已註冊的主機照常以 8443 連線，但沒有自動輪替、過期續簽與除役通知 |
| **Kairos 0.5，註冊通道關閉** | 與 0.4 相同（`hello_ack` 不列出新 feature、不帶新欄位；`renew_request`／`deregister` 回 `unknown_type`） | 與 0.4 相同 | 新註冊：`PLS-E2007`；已註冊的主機：自動輪替（8443）與除役通知照常，過期續簽 `grace_blocked`（`PLS-E2007`，請改為「僅續簽」） |
| **Kairos 0.5，`open`／`renew_only`** | 與 0.4 相同 | 與 0.4 相同 | 全部功能（`renew_only` 時不受理新註冊：`PLS-E2008`） |
| **Kairos 0.5，`storage.backend = "jsonl"`** | 與 0.4 相同 | 與 0.4 相同 | 沒有註冊服務（不監聽 8444、不列出 `renew`／`deregister`），同「Kairos 0.4」 |

- 驗證方式：`kairos-proto` 以凍結的舊版型別定義驗證舊端看到新訊息的行為；端對端測試 `tests/compat.rs` 以送出 0.1／0.4 Pulse 訊息的假 client 驗證 0.5 Kairos 的 `hello_ack` 欄位與 `unknown_type` 不變；`tests/enroll_pulse.rs` 以 0.4 設定的 Kairos 驗證新版 Pulse 的 `PLS-E2007`／`PLS-E2106` 與手動憑證的主機不受影響。

### 已知限制

- Kairos 未實作：手動簽發憑證的匯入（`kairos pki certificates import`）、離線模式的 CRL 工具與上傳、撤銷清單匯出、下載端點（`/agent/…` 回 404）、L11 下載限制、放寬設定與解除封鎖的管理端點、系統健康的整合（S20）、列表的游標分頁。見 PKI.md 第 13 節。
- Pulse：`pulse enroll` 在 `server-info`／`token-check` 階段收到 429（同一來源的請求太多）時直接結束（結束代碼 6，`PLS-E2202`），不依 `Retry-After` 等候；送出申請之後的 429 才會排隊。

### 測試

- 端對端測試（`tests/e2e`）新增 `tests/enroll.rs`（測試用的 HTTPS 用戶端打 8444：核准、長輪詢、8443 輪替、撤銷、除役、過期續簽與重放跨重啟、時鐘偏差、模式、限流）與 `tests/enroll_pulse.rs`（實際的 `pulse enroll`／服務／`pulse decommission`：ENROLL.md 第 19.3 節 #1～#5、#7～#12、#15～#20），以及 `tests/compat.rs` 的 0.4 線上格式測試。測試掛鉤（只在 debug 建置生效）：Kairos 的 `KAIROS_TEST_CERT_LIFETIME_SECS`、`KAIROS_TEST_AUTO_PAUSE_SECS`、`KAIROS_TEST_DROP_ENROLL_RESPONSES`、`KAIROS_TEST_DROP_RENEWAL_RESPONSES`，Pulse 的 `PULSE_TEST_GRACE_DELAY_SECS`。

### 發佈與建置

- Kairos 發佈包加入 `PKI.md`；Pulse 發佈包的 `pulse.service` 多了 `-/opt/pulse/identity`。
- 新的依賴：Pulse 直接依賴 `rcgen`（MIT OR Apache-2.0，以 ring 為後端）與 `ring`；Kairos 直接依賴 `rcgen`、`x509-parser`（以 ring 驗證 CSR 與憑證）、`time`。都原本就在依賴樹中，`cargo deny check` 通過。

## [0.4.0] - 未單獨發佈（內容隨 0.8.0 發佈）

**Kairos 0.4.0、Pulse 0.4.0**：商業審計路線圖「階段 3 MVP 產品面」。Kairos 新增 F1（查詢與管理 API）第一版：HTTPS 的主控台 API（`/api/v1`，唯讀）與 API token，是主控台畫面的前提；Pulse 新增 `pulse status`、所有人可讀的本機狀態檔與日誌的事件代碼。**協定不變**（仍為 v1～v2＋`durable`），兩個產品可以分批、以任何順序升級。API 的規格、端點、驗證、即時推送與量測見 `kairos/API.md`（Kairos 發佈包內的 `API.md`）；Pulse 的錯誤代碼對照見 `pulse/ERROR-CODES.md`（Pulse 發佈包內的 `ERROR-CODES.md`）。

### 破壞性變更與升級步驟（升級前必讀）

| # | 產品 | 變更 | 誰會受影響 | 升級步驟 |
|---|---|---|---|---|
| 1 | Kairos | **預設監聽主控台 API `127.0.0.1:9443`**（`[console]`，HTTPS；預設啟用、只綁 loopback） | 9443 已被其他程式使用的主機；與 `server.listen` 使用同一個位址與連接埠的設定 | 升級前 `ss -ltnp \| grep :9443` 確認沒有被使用。被佔用時 Kairos 以結束代碼 11 無法啟動（systemd 每 5 秒重試）：在 `kairos.toml` 加上 `[console]` 的 `listen`（換連接埠）或 `enabled = false`。與 `server.listen` 相同時以 `KRS-E0202` 拒絕啟動（結束代碼 3） |
| 2 | Kairos | **資料庫 schema 升級到 v3**（`api_tokens`、`hosts.cert_rejected_at`／`cert_rejection`） | 所有 SQLite 部署 | 升級前 `kairos db backup`（Kairos INSTALL 第 11 節）；升級後 0.3 無法開啟資料庫（降版從備份還原） |
| 3 | Kairos | **檔案描述元的需要量多了主控台的連線上限**：`server.max_connections` ＋ `console.max_connections`（預設 64）＋ 64（設計包 D21） | 自訂 `LimitNOFILE` 剛好等於舊算式的部署 | 啟動時不足會記 `WARN` 並附建議值；發佈包 unit 的 `LimitNOFILE=8192` 足夠 |
| 4 | Pulse | **本機狀態檔改到 `/var/lib/pulse/status.json`，需要新版 `pulse.service`**（`StateDirectory=pulse`、`StateDirectoryMode=0755`；多實例 `StateDirectory=pulse/N`） | 以發佈包的 systemd unit 執行的部署（`ProtectSystem=strict` 下 `/var/lib/pulse` 不可寫）；要讓一般帳號執行 `pulse status` 的主機 | 換上新版 unit 並 `systemctl daemon-reload`（多實例依 Pulse INSTALL 第 9 節重新產生）；`chmod 751 /opt/pulse`（多實例的 `/opt/pulse-N` 也是）。沒有換 unit 時**服務照常啟動與監控**：狀態檔改寫在 `/opt/pulse/logs/status.json` 並記一次 `WARN`（`status.write_failed PLS-E8204`），`pulse status` 只有 root 能用，「設定」列為警告（結束代碼 8） |
| 5 | Pulse | **日誌訊息開頭加上事件與代碼**（例如 `buffer.full PLS-W4202 …`、`conn.registered - …`），`clock.skew` 超過 5 分鐘改為 `ERROR`；啟動失敗的 stderr 改為 `錯誤 PLS-E0101：…` | 以行首比對 Pulse 日誌文字的告警規則或腳本（例如比對 `[ERROR] 連線中斷`） | 升級前把規則改為比對事件或代碼（`conn.lost`、`PLS-E2002` 等，`pulse/ERROR-CODES.md`），或改用包含比對 |

**升級步驟**（依序；協定不變，也可以先升級 Pulse，Kairos 與 Pulse 0.3／0.4 可以任意混用）：

1. **Kairos 升級前**：`kairos db backup` 備份資料庫（第 2 項），確認 9443 沒有被使用（第 1 項）。
2. **升級 Kairos**：換上新版執行檔並重新啟動（Kairos INSTALL 第 11 節，unit 不需修改）→ 確認日誌有「主控台 API 已啟動，監聽 https://127.0.0.1:9443/api/v1/」。
3. **確認主控台 API**：以 `kairos api-token create` 建立 token（預設 90 天後到期）並依 Kairos INSTALL 第 9.3 節確認；需要從其他主機使用時把 `console.listen` 改為對外位址，並以防火牆**只允許管理網段**。
4. **Pulse 升級前**：依第 5 項調整比對 Pulse 日誌文字的告警規則。
5. **每台 Pulse 主機：換上新版 `pulse.service`**（第 4 項；多實例時對每個實例重新產生）並 `systemctl daemon-reload`；`chmod 751 /opt/pulse`。
6. **升級 Pulse**：換上新版執行檔並重新啟動（Pulse INSTALL 第 10 節），以 `/opt/pulse/pulse status` 確認（結束代碼 0；「設定」列沒有 `PLS-E8204`）。

### Kairos

#### 新增

- **主控台 API**（F1；設計包 `screens.md` §24 #1、#4、#5、#19）：HTTPS（rustls／ring，HTTP/1.1，hyper 1），版本前綴 `/api/v1`，JSON，時間為 RFC 3339 UTC 毫秒，每個回應帶 `server_time`，錯誤為 `{"error":{"code","product_code","message"}}`（`product_code` 對照 `copywriting.md` 的 `KRS-` 代碼），游標分頁。端點：
  - `GET /api/v1/hosts`：主機清冊列表，伺服器端篩選（狀態、從未連線、失聯超過 N、離線原因、tag、OS、版本、是否支援補送、CIDR、主機名稱／顯示名稱／IP／agent_id 搜尋）、排序（嚴重度、名稱、最後上線、IP、版本、CPU、記憶體）、游標分頁、總數與各狀態台數；
  - `GET /api/v1/hosts/{agent_id}`：詳情（清冊欄位、主機指紋、OS、標籤、目前連線、最近一次 report、是否協商補送與補送中、握手時被拒的憑證）；
  - `GET /api/v1/hosts/{agent_id}/metrics[/{name}]`：目前回報的指標、趨勢（依範圍自動選原始資料／5 分鐘／1 小時彙總，也可指定；min／max／avg／count；有 label 的序列分開；點數上限；尚未寫出的彙總由原始資料計算）；
  - `GET /api/v1/overview`：各狀態台數、每分鐘上線數（24 小時）、與 1 小時前的差、離線最久、資源使用前 10、健康摘要；
  - `GET /api/v1/system/health`：資料庫用量與容量上限、以目前回報中的主機與宣告的間隔估算保留期滿的用量、最近一次容量刪除、保留設定與最近一次清除、report 寫入統計（成功、`storage_unavailable`、結果不明、重複、速率限制）、支援補送／補送中的台數、連線使用率、TLS 到期、監聽位址、檔案描述元、驗證模式；
  - `GET /api/v1/events`：即時變更推送（SSE）：`host.added`、`host.online`、`host.offline`（附原因），每 15 秒心跳，`Last-Event-ID` 在保留範圍（4,096 則）內補送、否則 `resync`，同時連線數上限 `console.max_streams`；
  - `GET /api/v1/me`：目前的身分。
- **API 錯誤的產品代碼**（設計包 `copywriting.md` §3.4、`decisions/INTEGRATION.md` §26）：401 `unauthorized` → `KRS-E6010`、429 `rate_limited` → `KRS-W6011`、403 `forbidden` → `KRS-E6104`；`storage_unavailable`／`internal` → `KRS-E4003`；用法錯誤與暫時狀態為 `null`。每個來源第一次被限速時的 `WARN` 日誌為「警告 KRS-W6011：…」。
- **API token**（暫行驗證，F6 之前）：`kairos api-token create|list|revoke`（設計包 `cli-style.md` §11.6；token 只在建立時顯示一次、資料庫只存 SHA-256、唯讀範圍、撤銷立即生效）。有效期限**預設 90 天、上限 3,650 天**（`--expires-in`／`--expires-at`），**不提供不過期的 token**（需要長期使用時 `--expires-in 3650d`）。給人看的時間用本機時區並標示時區（`--json` 為 UTC）；`list` 對 14 天內到期的有效 token 加 `⚠`（ASCII `[WARN]`）並提示輪替。`Authorization: Bearer`；常數時間比對；**驗證失敗依來源 IP 速率限制**（IPv6 以 /64 合併；可連續 10 次，之後每 6 秒恢復 1 次，額度用完回 429）；最後使用的時間與來源每 60 秒寫回；建立與撤銷由執行中的 Kairos 寫入日誌。
- **狀態暖機期**（設計包 `ia.md` §5.1）：Kairos 啟動後 2 分鐘內，最後一次訊息在這次啟動之前、還沒連上的主機為 `unknown`（`status_detail.starting`），之後仍未連上改為 `offline`。
- **設定 `[console]`**：`enabled`（預設 `true`）、`listen`（預設 `127.0.0.1:9443`）、`cert_path`／`key_path`（預設沿用伺服器憑證，熱重載）、`max_connections`（64）、`max_streams`（16）。
- **握手時被拒的憑證歸屬到主機**（設計包 §24 #3）：已撤銷、已過期（或無法判定撤銷狀態）的客戶端憑證以 SAN 歸屬到 `agent_id`，記錄原因（`KRS-W1105`／`W1108`／`E1203`）、序號與來源位址，寫入主機清冊；主機詳情與列表的離線原因顯示「憑證被拒」。不受信任的 CA 簽發的憑證不歸屬（SAN 可偽造）。
- **`KRS-W4202`**（容量上限刪除最舊的資料；設計包 §25.5 #3）：日誌改用代碼與 `copywriting.md` 的文案，開始與結束寫入 `meta.capacity_event`，系統健康顯示最近一次容量刪除。
- 量測（`kairos/src/api/bench.rs`，kp-build 4 vCPU）：1 萬台主機的列表 p99 約 10 ms（每次重建索引的最壞情況約 40 ms）、30 天趨勢 p99 約 8～13 ms；每秒 2,000 筆 report 同時以主控台一般用量查詢時 `ack` p99 約 1 ms（只有寫入時約 2 ms），連續查詢的壓力下約 6 ms（API.md 第 9 節）。

#### 變更

- 啟動時的檔案描述元檢查（`LimitNOFILE`）的需要量含主控台的連線上限（上方第 3 項）。
- 狀態表記錄最近一次的離線原因（逾時、連線關閉、Kairos 關機、連線中憑證被拒）、是否協商了 `durable` 與最近一次收到補送資料的時間，供 API 使用；`agents.json` 的格式不變。
- 主控台在自己的 tokio runtime 上執行，查詢不影響 agent 連線與 report 寫入的延遲。
- `kairos hosts list`、`reports export／import`、`db backup` 不變；`kairos reports import` 仍需要停止 Kairos（主控台的寫入連線不會放掉跨行程的檔案鎖，e2e `import.rs` 涵蓋）。
- 資料庫在執行中被替換、writer 重新開啟時，不再以 `std::fs` 開關既有的資料庫檔與 `-wal`／`-shm`（只以路徑收緊權限）：同一個行程內關閉這些檔的 fd 會放掉其他連線（主控台的唯讀連線池、背景 checkpoint）的 fcntl 鎖，`kairos reports import` 可能誤判 Kairos 未執行。
- `storage.backend = "jsonl"` 時不啟動主控台（記 `WARN`）。

### Pulse

#### 新增

- **`pulse status`**（設計包 `pulse/cli-spec.md` §4）：讀取本機狀態檔與服務管理員（systemd、Windows 服務控制管理員），顯示服務、連線、回報、身分、設定、時鐘六列與本機狀態（線上／警告／離線／嚴重／未知），問題另以三段式說明（標題、原因、處理方式與代碼）寫到 stderr。`--brief`（單行）、`--json`（`pulse.status/v1`，純 ASCII）、`--watch [秒]`、`--exit-codes=nagios`、`--config`、`--ascii`、`--color`。不需要連線；結束代碼 0 線上、8 警告、9 離線／嚴重／未知、2 用法錯誤、10 無法讀取狀態檔。「回報」列寫出補送資訊：待補送筆數、最舊一筆的取樣時間、預計補完時間、最近 24 小時內因緩衝已滿或超過保存時間而丟棄的筆數與取樣時間範圍（`PLS-W4202`／`PLS-W4203`）、無法寫入緩衝（`PLS-W4103`）、緩衝無法使用（`PLS-E4004`）、Kairos 暫時無法保存（`PLS-W4101`）。README「本機狀態（pulse status）」、INSTALL 第 8.2 節。
- **本機狀態檔**（`cli-spec.md` §4.1）：Linux `/var/lib/pulse/status.json`（`pulse.service` 新增 `StateDirectory=pulse`；多實例 `StateDirectory=pulse/N` 時依 `STATE_DIRECTORY` 寫 `/var/lib/pulse/N/`），Windows `%ProgramData%\Pulse\status.json`；`[status] file` 可改。**所有人可讀**（Unix 0644、目錄 0755，不受 unit 的 `UMask=0077` 影響；Windows 授予 Users 讀取），`pulse status` 不需要 root。執行中每 10 秒、狀態變化時另外以原子方式改寫（暫存檔＋rename，不 fsync）。內容不含秘密：沒有私鑰、token；未啟用 mTLS 時不含 agent_id（等同身分憑據）。以單調時鐘判斷是否過時（超過 60 秒為「未知」，`PLS-W8004`），正常結束前寫入結束代碼。
- **`pulse explain <代碼>`**：印出錯誤代碼的標題；發佈包新增 `ERROR-CODES.md`（代碼 → 程式位置 → 觸發條件的對照表，與尚未實作的代碼）。
- 子命令 `pulse run`（等同不帶參數）；`pulse status --instance <N>`（多實例）。

#### 變更

- **日誌加上固定事件與錯誤代碼**（上方第 5 項；設計包 `pulse/log-ux.md` §4、`decisions/INTEGRATION.md` §25.5）：有對應事件的行，訊息開頭為「`<event> <code> `」，例如 `buffer.full PLS-W4202`、`auth.rejected PLS-E1105`、`conn.registered -`。本機緩衝的 5010～5015（`buffer.opened`、`buffer.unavailable` `PLS-E4004`、`buffer.write_failed` `PLS-W4103`、`buffer.full` `PLS-W4202`、`buffer.expired` `PLS-W4203`、`buffer.replay_done`），連線中斷依原因分為 `conn.lost`（`PLS-E2002`～`E2005`、`E2201`）、`tls.server_cert_rejected`（`PLS-E1401`～`E1405`）、`auth.*`、`conn.superseded`、`proto.incompatible`、`server.full`；新增 `conn.restored`（恢復時記中斷時間、重試次數、待補送與丟棄筆數）與 `conn.outage`（中斷滿 30 分鐘時一次 `ERROR`，同時寫 stderr）。
- **`clock.skew`**（事件 5000，`log-ux.md` §4.5）：30 秒～5 分鐘記 `WARN`（`PLS-W8301`），**超過 5 分鐘改記 `ERROR`**（`PLS-E8302`）並寫 stderr；只在等級改變或偏差再變化超過 1 分鐘時記錄（重連時不重複），回到 30 秒以內記一則 `INFO`（`clock.skew -`）。
- **`status.write_failed`**（事件 5020，`PLS-E8204`）：執行中無法寫入狀態檔時第一次記 `WARN`，連續失敗滿 60 秒（`pulse status` 開始顯示「未知」）記 `ERROR` 並寫 stderr，之後每 10 分鐘一則 `ERROR` 摘要、其間 `DEBUG`；恢復寫入記 `status.write_restored`（事件 5021，`INFO`）。
- **`pulse status` 遇到過時的狀態檔**（`cli-spec.md` §4.8）：連線、回報、身分、時鐘列以最後已知內容照常判定，並標示「（最後已知，N 分鐘前）」，沒有內容時略過（`skipped`）；本機狀態為「未知」（結束代碼 9）。沒有權限讀取狀態檔時結束代碼 10，`--exit-codes=nagios` 為 3（UNKNOWN）。
- 啟動失敗的 stderr 訊息帶代碼：`錯誤 PLS-E0101：無法載入設定：…`（範圍錯誤為 `PLS-E0201`）、`錯誤 PLS-E4002：無法初始化日誌：…`；`agent.fatal` 帶 `PLS-E3103`／`PLS-E3104`（`agent.id`）、`PLS-E1102`（SAN 與 agent_id 不符）。命令列錯誤帶 `PLS-E0001`～`E0003`。
- 無法開啟本機緩衝時，systemd 的 `Status:` 在「已註冊：…」後加上「；無法開啟本機緩衝（PLS-E4004，斷線期間的資料不會補送）」。
- `--help` 列出 `status`、`explain`。

#### 與設計包的差異與待辦

見 `pulse/ERROR-CODES.md` §4、§5：尚未實作 Windows 事件記錄（`clock.skew`、`status.write_failed` 的 `ERROR` 目前只寫日誌與 stderr）、服務描述、`log-ux.md` 的完整格式（RFC 3339 時間、JSON）與重試摘要、`--all`、`--json-lines`，以及 53 個與註冊、安裝、自動更新憑證相關的代碼。Windows 的 MSI（含 SCCM 偵測指令碼依賴的 `install-result.json` 與執行檔的版本資源）尚未實作，在那之前不要以 `Detect-PulseInstalled.ps1` 作為必要部署的偵測規則（設計包 `sccm-deployment.md` §3、`decisions/INTEGRATION.md` §26.5 #8）。

### 相容性

- 協定不變：Kairos 0.4 與 Pulse 0.2／0.3／0.4 的組合，以及 Pulse 0.4 與 Kairos 0.2／0.3 的組合，都與對應的 0.3 組合相同（見「版本相容矩陣」與 0.3.0 的「相容性（A5）」）。
- `pulse status` 只讀本機狀態檔，與連上的 Kairos 版本無關。

### 已知限制

- API 為唯讀；沒有使用者、角色、工作階段（F6）、告警（F3）與網頁介面（F2）。主機狀態只有 `online`、`offline`、`unknown`，`critical`／`warning`／`pending` 的計數為 `null`。其他限制、延後項目與待決定事項見 `kairos/API.md` 第 10 節。

### 共用

- 端對端測試（`tests/e2e`）新增 `api.rs`（以實際的 `kairos api-token create` 建立 token，經主控台 API 查主機與指標，SSE 收到上線／離線事件，撤銷的憑證歸屬到主機）與 `status.rs`（`pulse status` 隨 Kairos 停機、恢復與 Pulse 結束的變化）。每個 Kairos 另取一個隨機埠給主控台；子行程不繼承 `PULSE_SYMBOLS`、`NO_COLOR`、`CLICOLOR_FORCE`，狀態檔寫在各測試自己的暫存目錄。

### 發佈與建置

- 版本：`kairos`、`pulse` 改為 0.4.0；發佈包為 `kairos-0.4.0-<commit>-linux-x86_64.tar.gz`、`pulse-0.4.0-<commit>-linux-x86_64.tar.gz`。
- Kairos 發佈包另附 `API.md`；Pulse 發佈包另附 `ERROR-CODES.md`，`pulse.service` 新增 `StateDirectory=pulse`、`StateDirectoryMode=0755`。
- 新增依賴 `hyper`（只啟用 `server`、`http1`）、`hyper-util`（`tokio`）及其依賴 `http`、`http-body`、`httparse`、`httpdate`、`atomic-waker`（皆為 MIT／Apache-2.0）；`ring`、`subtle` 改為 Kairos 的直接依賴（原已在依賴樹中）。`deny.toml`、`packaging/about.toml` 的授權允許清單不需變更。

## [0.3.0] - 未單獨發佈（內容隨 0.8.0 發佈）

**Kairos 0.3.0、Pulse 0.3.0**：商業審計路線圖「階段 2 資料層」中的 A1（資料庫）、A5（at-least-once 投遞）、A6（持久化主機清冊）、A9（容量）。Kairos 的 report 預設改寫 SQLite，另有主機清冊、5 分鐘／1 小時彙總、分層保留與總容量上限；Pulse 新增本機環狀緩衝，Kairos 停機或網路中斷期間的取樣在恢復後補送，雙方以協定 feature `durable` 協商，Kairos 以唯一索引去重。架構、schema、設定、容量估算、量測與備份見 `kairos/STORAGE.md`（發佈包內的 `STORAGE.md`）；協定見 `crates/kairos-proto/PROTOCOL.md`「durable：at-least-once 投遞」。

### 破壞性變更與升級步驟（升級前必讀）

| # | 產品 | 變更 | 誰會受影響 | 升級步驟 |
|---|---|---|---|---|
| 1 | Kairos | **report 預設寫入 SQLite（`data/kairos.db`），不再寫 `reports-YYYY-MM-DD.jsonl`**（A1） | 直接讀取 `data/reports-*.jsonl` 的工具（匯入、報表、監控腳本），以及複製資料目錄的備份腳本 | 升級前擇一：讀取工具改用 `kairos reports export --since <時間> [--agent <agent_id>]`（每行格式與 JSONL 相同，`peer`、`hostname` 為清冊中最後的值）；或在 `[storage]` 加上 `jsonl_mirror = true` 同時輸出 JSONL（盡力而為、不影響 `ack`，磁碟用量約多 6 倍）；或暫時設定 `backend = "jsonl"`（與 0.2 相同，沒有主機清冊與彙總）。備份改用 `kairos db backup <檔案>`（**執行中不可直接複製 `kairos.db`**，INSTALL 第 10.2 節）。既有的 JSONL 檔不會自動匯入資料庫、不會被修改；需要舊資料時以 `kairos reports import` 匯入（需要停止 Kairos，見下方升級步驟 2a）。不開鏡像時 JSONL 的保留清除不再執行，舊檔請在匯入後自行搬移或刪除 |
| 2 | Kairos | **資料目錄必須在本機磁碟** | `storage.data_dir` 放在 NFS、SMB 等網路磁碟的部署 | SQLite 的 WAL 需要共享記憶體與檔案鎖；無法啟用 WAL 時拒絕啟動（`無法啟用 WAL`）。改到本機磁碟 |
| 3 | Kairos | **總容量上限預設 64 GiB，超過時刪除最舊的資料**（A9） | 主機多（約 1,300 台以上）或保留天數調長的部署 | 依 STORAGE.md「容量估算」設定 `storage.max_total_bytes`（0 = 不限制）與 `raw_retention_days`／`rollup_5m_retention_days`／`rollup_1h_retention_days`（預設 7／31／400 天）。上限小於以 `server.max_connections` 台估算的容量時啟動記 `WARN`。縮短 `raw_retention_days` 時一併檢查第 6 項的 `buffer.max_age_hours` |
| 4 | Kairos | **`agents.json` 在重新啟動後保留最近 24 小時內出現過的主機**（A6，行為改變） | 依「重新啟動後 `agents.json` 為空」或 `total` 計數的工具 | 重新啟動後這些主機以 `online: false`、`conn_id: 0` 出現，24 小時後照常移除；需要所有主機（含更早的）時用 `kairos hosts list --json` |
| 5 | Kairos | **補送的資料：`ts` 可能比 `recv_at` 早數小時到數天**（A5）。SQLite 中依 `ts`（Pulse 的取樣時間）保存、彙總與套用保留期限；匯出與 JSONL 另有選用欄位 `boot_id`、`report_seq`（`schema` 不變），JSONL 仍寫入收到當天的檔案 | 依 `recv_at`、檔案日期或匯出順序當作取樣時間的讀取工具 | 時序分析改用 `ts`；忽略未知欄位的工具不受其他影響。`kairos reports export --since` 依取樣時間篩選（STORAGE.md 第 5.2 節） |
| 6 | Pulse | **預設啟用本機緩衝，需要可寫的 `buffer/` 目錄**（A5） | 以發佈包的 systemd unit（`ProtectSystem=strict`，只有 `logs/` 可寫）執行的部署 | 升級前 `install -d -o pulse -g pulse -m 750 /opt/pulse/buffer`，並換上新版 `pulse.service`（`ReadWritePaths=/opt/pulse/logs -/opt/pulse/buffer`；多實例時對每個實例重做，Pulse INSTALL.md 第 9、10 節）。沒有做時 Pulse 照常監控，但記 `ERROR`「無法開啟本機緩衝」並改為 at-most-once。緩衝保留 `buffer.max_age_hours`（預設 168 小時）內的資料，建議不超過 Kairos `raw_retention_days` × 24（STORAGE.md 第 5.4 節） |
| 7 | Pulse | **磁碟用量增加**：緩衝預設最多約 72 MiB（`buffer.max_mb` 64 MiB 加兩個區段）（A5） | 磁碟空間很小的主機 | 在 `pulse.toml` 調小 `[buffer] max_mb`／`max_records`，或 `enabled = false`（與 0.2.0 相同） |
| 8 | 兩者 | **恢復連線後的寫入量暫時增加**：每台 Pulse 補送時約每秒 0.83 筆（`buffer.replay_interval_ms` 預設 1200；平常每 5 秒 1 筆），1 萬台同時恢復約每秒 8,300 筆（平常約 2,000 筆）（A5） | 大量主機同時恢復連線的部署（例如 Kairos 維護後） | kp-build（4 vCPU）上量測：每秒 8,000 筆持續 5 分鐘 p99 0.12 秒、沒有 `storage_unavailable`；每秒 16,000 筆持續 5 分鐘時約 3% 回 `storage_unavailable`（Pulse 退避後重送，不遺失、不重複），最長回應 5.05 秒（8 秒預算內）（STORAGE.md「量測」）。預設值即依此訂在約每秒 8,300 筆；主機超過 1 萬台時再調大 `buffer.replay_interval_ms`（例如 2000）。以 `backend = "jsonl"` 或鏡像寫 JSONL 時，補送寫入收到當天的檔案，`storage.max_daily_bytes` 可能較快達到（之後回 `storage_unavailable`，資料留在 Pulse，換日後繼續補送） |

**升級步驟**（依序；Kairos 先升級，Pulse 升級後的第一次斷線就能補送）：

1. **Kairos 升級前**：依第 1 項處理讀取 JSONL 的工具與備份腳本，依第 5 項檢查以 `recv_at` 當取樣時間的工具；確認資料目錄在本機磁碟並有足夠空間（第 2、3 項）。
2. **升級 Kairos**：換上新版執行檔並重新啟動（Kairos INSTALL 第 11 節，unit 不需修改）。之後每次升級 Kairos 前先以 `kairos db backup` 備份：新版可能升級 schema，升級後的資料庫無法給舊版使用。
   - 2a.（選用）**匯入 0.2 的 JSONL**：`kairos reports import --dry-run` 估算筆數後，停止 Kairos、`sudo -u kairos /opt/kairos/kairos reports import /opt/kairos/data --config /opt/kairos/kairos.toml`、再啟動（約每秒 3～4 萬行）。停機要短時，可在換上新版之前、0.2 仍在執行時，先以新版的執行檔匯入前幾天的檔案，停機時只匯入當天的（STORAGE.md「匯入舊的 JSONL」）。
3. **確認 Kairos**：以 `sudo -u kairos /opt/kairos/kairos hosts list --config /opt/kairos/kairos.toml` 確認既有的 Pulse（0.2.0，at-most-once）重新連上後出現在清冊中。
4. **每台 Pulse 主機：建立緩衝目錄**：`install -d -o pulse -g pulse -m 750 /opt/pulse/buffer`（第 6 項；多實例時每個實例各一個）。
5. **換上新版 `pulse.service`** 並 `systemctl daemon-reload`（`ReadWritePaths` 加上 `-/opt/pulse/buffer`）。
6. **升級 Pulse**：換上新版執行檔並重新啟動（Pulse INSTALL 第 10 節）。啟動日誌應有「本機緩衝：buffer（待送 0 筆…）」，Kairos 的上線日誌註明「durable：report 可補送、去重」。
7. 需要時調整 Pulse 的 `[buffer]`（第 7、8 項）與 Kairos 的保留天數，讓 `buffer.max_age_hours ≤ 24 × raw_retention_days`（第 6 項）。

先升級 Pulse 也可以：連上 0.2.0 的 Kairos 時不啟用 durable（連線期間 at-most-once），斷線期間的積壓保留在 Pulse 的緩衝中，Kairos 升級後才補送（只是會先佔用緩衝）。

### 協定（kairos-proto）

- 新增 feature `durable`（v2 的 `features` 協商，不提升協定版本）：雙方都宣告時 report 為 at-least-once，report 帶 `boot_id`（UUID，Pulse 每次程序啟動時產生）與 `report_seq`（同一 `boot_id` 內從 1 遞增）；缺漏或型別錯誤回 `bad_request`。任一端沒有宣告時維持 at-most-once。PROTOCOL.md 新增「durable：at-least-once 投遞」與測試向量，並更新相容矩陣、寫入結果不明時的重送語意與 8 秒回應預算的說明（A5）。
- `DurableSeq`、`FEATURE_DURABLE`、`Session.durable`、`validate_durable_seq`、`DURABLE_DEDUP_WINDOW`（256）、`MAX_DURABLE_IN_FLIGHT`（8）。
- `PROTOCOL.md`：`ack` 的「已寫入」涵蓋資料庫的 commit；狀態表與 `agents.json` 在重新啟動後的行為依主機清冊更新；伺服器的去重範圍分別說明 SQLite（原始資料保存期間）與 JSONL（記憶體視窗）。

### Kairos

#### 新增

- **SQLite 儲存**（A1，`storage.backend = "sqlite"`，預設）：`data/kairos.db`（WAL）。原始 report 每筆一列，指標以緊湊的二進位格式保存，名稱、單位與 label 的組合（序列）各存一次，不再每筆重複 `peer`、`hostname`、`agent_id`；以 `(host_id, ts)` 索引查詢某台主機的時間範圍。Pulse 0.2.0 的典型 report（17 個指標）每筆約 240 bytes（含索引），JSONL 約 1.5 KB。schema 以 `PRAGMA user_version` 管理版本，啟動時依序遷移（每版一個交易）；資料庫比程式新時、或不是 Kairos 的資料庫時拒絕啟動。
- **寫入介面抽象化**：連線只認得 `ReportSink`；後端實作 `trait ReportStore`（`store/mod.rs`），日後可換成時序資料庫。單一 writer 把佇列中目前所有的要求（最多 1024 筆）寫成一個交易（group commit），在 blocking 執行緒上執行，commit 成功後才回 `ack`；commit 前失敗、磁碟已滿等確定未寫入時回 `storage_unavailable`，commit 結果不明時維持「不回應、關閉連線」。checkpoint 由背景執行緒進行（必要時由 writer 在兩批之間補做），不在 commit 時擋住寫入。資料庫在執行中被刪除或替換時重新建立，不會寫進已刪除的檔案後回 `ack`。量測（kp-build，每秒 2000 筆、1 萬台主機）：p50 0.55 ms、p99 1.4 ms（STORAGE.md「量測」）。
- **durable（at-least-once）與去重**（A5）：協商 `durable` 時，連線以 `ReportRecord::with_dedup(Some(DedupKey { boot_id, report_seq }))` 把去重鍵交給 writer（`ReportSink::submit(record)` 不變）。SQLite 以唯一索引保證同一主機、同一個 `(boot_id, report_seq)` 只保存一次，範圍為原始資料保存的整段期間；重複的回 `WriteStatus::Duplicate`，連線照常回 `ack`、不重寫、不加入彙總、不寫入鏡像、不更新 `last_report`。commit 結果不明而斷線後的重送也由唯一索引去重，記憶體中的彙總由原始資料重建，那一筆只算一次。`backend = "jsonl"` 時以記憶體中的視窗去重（每個 boot 256 筆，啟動時由最新兩個 report 檔尾端重建，48 小時閒置清除）；JSONL 鏡像不另外去重。上線日誌註明「durable：report 可補送、去重」。
- **補送的歷史資料**（A5）：一律接受，`ts` 原樣保存。SQLite 的 `reports.ts` 為取樣時間（durable 時為 Pulse 的 `ts`，須在收到時間的 −367 天～＋5 分鐘內，否則用收到的時間；非 durable 為收到的時間），另存 `recv_lag` 還原收到的時間。彙總依取樣時間分桶；取樣時間落在已寫出的桶時，在同一個交易中累加進 `rollup_5m`／`rollup_1h`（不覆蓋、不遺漏、重新啟動後不重複）；補送中的桶等補完（15 秒沒有新值）才寫出。`agents.json` 的 `last_report` 不會被較舊的補送資料蓋過。
- **主機清冊**（A6）：所有出現過的主機保存在 `hosts` 表（`agent_id`、hello 的主機資訊含 `host_fingerprint`、最後的來源位址、是否以 mTLS 驗證身分、首次出現、最後訊息、最後一筆 report），每 30 秒從狀態表同步（每個交易最多 500 列，其餘在之後的維護步驟寫入；1 萬台時每個交易約 17 ms）、關機時寫入最後狀態。啟動時把上次異常結束時仍標為線上的主機改為離線，並把最近 24 小時內出現過的主機以離線狀態放回狀態表（「重新上線」與主機指紋的比對延續到重新啟動之後）。新設定 `storage.max_hosts`（預設 `max(100000, limits.max_agents)`）。
- **`kairos hosts list [--offline-for <時間>] [--wide] [--no-header] [--json] [--ascii]`**：查詢主機清冊（線上／離線／未知、主機名稱、`agent_id`、IP、最後訊息），回答「哪些主機已失聯 N 天」；`--json` 為純 ASCII（`schema` `kairos.hosts.list/v1`），錯誤為 `KRS-E4003`。輸出依設計包 `cli-style.md`。
- **`kairos reports export [--since <時間>] [--agent <agent_id>]`**：以 JSON Lines 匯出 SQLite 中的原始 report，格式與 JSONL 檔相同（`schema` 1／2，durable 的另有 `boot_id`、`report_seq`；`--since` 依取樣時間）。
- **`kairos db backup <檔案>`**：線上備份（`VACUUM INTO`），Kairos 執行中也可以執行，不覆寫既有檔案，備份檔 `0640`。
- **`kairos reports import [--dry-run] [--since <時間>] [--until <時間>] [--json] <檔案或目錄>...`**：把 0.2.x 以前的 `reports-YYYY-MM-DD.jsonl`（0.1.0 沒有 `schema` 的行、`schema` 1／2、durable 的 `boot_id`／`report_seq`）匯入 SQLite（STORAGE.md「匯入舊的 JSONL」）。
  - 取樣時間與 Kairos 寫入時的規則相同（durable 的行用 `ts`，其他用 `recv_at`），`reports export` 輸出的行與原本的相同。
  - **冪等**：以「主機＋取樣時間＋內容雜湊」為去重鍵（新的 `import_keys` 表），重複執行、中斷後重新執行、同一行出現在兩個檔案都只匯入一次；durable 的行另以原本的唯一索引、非 durable 的行以「資料庫中已有完全相同的原始 report」（例如 JSONL 鏡像）判斷重複。
  - **保留期限**：原始資料保留期限內的寫入原始資料並併入彙總；超過的**只併入彙總**（5 分鐘彙總也過期時只進 1 小時）；超過所有彙總保留期限的略過。彙總與 Kairos 的遲到資料相同的方式合併，並納入 Kairos 停止時尚未寫出的彙總，Kairos 再啟動時不重複計算。
  - **主機**：清冊中沒有的主機以離線、`hosts.source = 'import'` 建立；既有的主機只在匯入的資料較新時更新 `last_seen`、`hostname`、`peer`。
  - **需要停止 Kairos**：以 SQLite 的獨占鎖開啟資料庫，Kairos 執行中時以結束代碼 11（`KRS-E4304`「資料庫正在使用中」）拒絕，匯入期間 Kairos 也無法啟動；不與服務同時寫入（服務記憶體中的彙總與 8 秒回應預算）。`--dry-run` 以唯讀方式開啟，執行中也可以。
  - 來源檔只讀取；目錄只取 `reports-*.jsonl` 與 `reports.jsonl`，不進入子目錄、不跟隨符號連結（參數本身是符號連結時也拒絕），壓縮檔列出提示先解壓縮；Linux 上以 root 執行而資料目錄屬於其他帳號時拒絕（結束代碼 10）。
  - 壞行（不是 JSON、欄位缺漏、不合法、沒有換行的最後一行）略過並計數，每個檔案列出前 5 個的行號與原因；摘要列出讀取、匯入（原始資料／只進彙總）、重複略過、壞行、超出保留期、不在範圍、無法匯入與新增主機。結束代碼：0 完成（含有壞行）、1 有來源無法讀取或匯入失敗、2 用法、3 設定、10 權限、11 資料庫使用中。`--json` 為純 ASCII（`schema` `kairos.reports.import/v1`）。
  - kp-build 上 100 萬行（984 MB）約 25 秒（每秒約 4 萬行；1 萬台主機時約 3.4 萬行、只進彙總時約 2.1 萬行），再匯入（全部重複）約 15 秒；去重鍵每行約 23 bytes（STORAGE.md「量測」）。
- schema v2：`hosts.source` 與 `import_keys`（遷移在啟動或匯入時自動套用；以 0.3.0 開發中的建置建立的 v1 資料庫照常升級）。
- **彙總**（A9）：每個指標的 5 分鐘與 1 小時 min／max／sum／count（`rollup_5m`、`rollup_1h`）；v1 的三個欄位對應到標準指標（記憶體換算成 bytes），與 v2 合併成同一條趨勢。重新啟動時從原始資料補回尚未寫出的部分，不漏算、不重複算。
- **分層保留與總容量上限**（A9）：`storage.raw_retention_days`（預設 7）、`storage.rollup_5m_retention_days`（31）、`storage.rollup_1h_retention_days`（400），依取樣時間；`storage.max_total_bytes`（預設 64 GiB）超過時依序刪除最舊的原始 report、5 分鐘彙總、1 小時彙總，直到低於上限的 95%。刪除都分批進行（每步最多 5000 列、一個短交易），不會長時間鎖住資料庫；依時間的刪除沿用 DateGuard 的時鐘跳躍保護（狀態存在資料庫中）。
- 新設定 `storage.backend`（`sqlite`／`jsonl`）、`storage.jsonl_mirror`（`sqlite` 時另輸出 JSONL 鏡像，沿用 `retention_days`、`max_daily_bytes` 與保留清除）、`storage.sqlite_synchronous`（`normal`／`full`）。範例設定與 README 的設定表已更新。
- 啟動時：`storage.max_total_bytes` 小於以 `server.max_connections` 台主機估算的容量時記 `WARN`；記錄資料庫的 schema 版本、主機與序列數與載回的主機數。

#### 變更

- **（破壞性）** report 預設寫入 SQLite（見上表第 1 項）。`storage.retention_days`、`storage.max_daily_bytes` 只用於 JSONL（`backend = "jsonl"` 或鏡像）；鏡像達到 `max_daily_bytes` 時只是不寫鏡像，不回 `storage_unavailable`。`storage.max_daily_bytes` 的啟動警告只在寫 JSONL 時出現。
- **（行為改變）** `agents.json` 的處置：保留，格式不變，仍是狀態表的即時快照（每 5 秒），Kairos 不讀回這個檔案；完整的主機清冊改由 SQLite 保存（`kairos hosts list`）。重新啟動後 `agents.json` 含最近 24 小時內出現過的主機（見上表第 4 項）；`backend = "jsonl"` 時與 0.2 相同。
- JSONL 的保留清除與時鐘跳躍保護不變，對 `backend = "jsonl"` 與鏡像都有效。
- 新的日誌類別「SQLite 維護工作」（彙總寫出、保留期限與容量上限的刪除、清冊同步）的 `WARN` 依類別節流。

#### 文件

- 新增 `kairos/STORAGE.md`（發佈包內附）：架構、`ack` 語意、schema 與遷移、批次策略、去重、取樣時間與遲到資料、保留期限與 Pulse 緩衝的關係、主機清冊、彙總、保留與容量上限、設定、容量估算公式與表、吞吐量量測（含補送尖峰與匯入速度）、查詢範例、備份與還原、從 JSONL 升級（含 `kairos reports import` 的設計：去重鍵、保留期限、與服務並行、縮短停機的作法）。
- README：設定表、資料輸出、檔案權限、專案結構、依賴與目前限制依 SQLite 與 durable 更新；INSTALL：資料目錄須為本機磁碟、確認清單加上 `kairos hosts list`、備份改用 `kairos db backup`、升級前備份與回滾、疑難排解。

### Pulse

- 新增本機環狀緩衝 `[buffer]`（預設啟用）：取樣先寫進 `buffer/`，Kairos 確認後才刪除；重新連上後先依序補送積壓再送新的取樣，最多 4 筆在途、兩筆間隔至少 `replay_interval_ms`（預設 1200 ms，實際不超過 `report.interval` 的 60%，讓積壓一定補得完）；`storage_unavailable` 與結果不明時退避後重送，`invalid_report`／`bad_request` 丟棄（A5）。
- 緩衝滿時丟最舊並計數記 `WARN`；超過 `max_age_hours`（預設 7 天）的不送出；每筆記錄有 CRC，啟動時截掉斷電造成的不完整尾端；不逐筆 fsync（最多每 10 秒）。Unix 上目錄 0750、檔案 0640，目錄權限過寬時記 `WARN`（Windows 檢查 ACL）（A5）。
- 連上不支援 `durable` 的 Kairos 時，連線期間維持 at-most-once，緩衝中的積壓保留不送（A5）。
- `pulse.service` 允許寫入 `buffer/`（`ReadWritePaths=/opt/pulse/logs -/opt/pulse/buffer`）；INSTALL 補上緩衝目錄的建立與升級步驟。

### 相容性（A5）

| Kairos ＼ Pulse | Pulse 0.2.0 | Pulse 0.3.0（`buffer.enabled = true`） | Pulse 0.3.0（`buffer.enabled = false`） |
|---|---|---|---|
| Kairos 0.2.0 | 與 0.2.0 相同 | v2，不啟用 durable：連線期間 at-most-once；斷線期間的積壓保留在 Pulse，連上 0.3.0 的 Kairos 後才補送 | 與 0.2.0 相同 |
| Kairos 0.3.0 | v2，不啟用 durable（與 0.2.0 相同；Pulse 不宣告 durable）；資料寫入 SQLite | **v2＋durable：at-least-once** | 與 0.2.0 相同；資料寫入 SQLite |

協定 v1（0.1.0 的任一端）時不使用 features，維持 at-most-once。

### 容量估算（A5，預設值）

- 每台 Pulse 補送速率 1 ÷ 1.2 秒 ≈ 0.83 筆／秒（Kairos 的 report 速率限制為每秒 2 則、可連續 10 則，新連線先給 2 則；兩筆至少間隔 1.2 秒時 token 在下一筆之前已補回 2.4 則，連新連線的 2 則也不會用完，因此不會觸發限流或 `storage_unavailable`；設定的下限 500 ms 時恰為每秒 2 則，同樣不觸發），扣掉同時產生的新取樣（每 5 秒 1 筆）後積壓以約 0.63 筆／秒減少：斷線 1 小時（720 筆）約 19 分鐘補完，24 小時（17,280 筆）約 7.6 小時補完。
- 1 萬台同時恢復連線：Pulse 的重連隨機延遲（0～5 秒）與補送前的隨機延遲（0～5 秒）把開始時間攤開後，Kairos 的 report 寫入約 8,300 筆／秒（平常約 2,000 筆／秒，約 4.2 倍），持續到各台補完。SQLite 在 kp-build（4 vCPU）上：每秒 8,000 筆持續 5 分鐘 p99 0.12 秒、沒有 `storage_unavailable`；每秒 16,000 筆在 60 秒內 p99 0.1 秒，持續 5 分鐘時資料庫變大、寫入跟不上，約 3% 的 report 在 5 秒後回 `storage_unavailable`，最長回應 5.05 秒，沒有結果不明，8 秒回應預算不受影響（STORAGE.md「量測」）。伺服器端不會無限積壓：每條連線最多 4 筆在途、Kairos 逐則處理；寫入跟不上時寫入佇列（1024 筆）滿、連線等待逾時回 `storage_unavailable`，Pulse 退避（2 秒加倍到 60 秒）後重送，整體自動放慢。預設值 1200 ms 的尖峰（約 8,300 筆／秒）接近每秒 8,000 筆、沒有 `storage_unavailable` 的量測範圍。需要再壓低尖峰時調大 `buffer.replay_interval_ms`（例如 2000 ms → 約 5,000 筆／秒，24 小時的積壓約 16 小時補完）；主機少、希望補得快時可調小（500 ms → 每台約 2 筆／秒，24 小時的積壓約 2.7 小時補完）。
- 補送的資料不增加長期的容量（補的是斷線期間本來就該有的資料）；寫 JSONL 時寫入收到當天的檔案，`storage.max_daily_bytes` 可能比平常更快達到（見上表第 8 項）。

### 共用

- 端對端測試（`tests/e2e`）改以預設的 SQLite 執行：report 以實際的 `kairos reports export` 從資料庫取出檢查，另檢查 JSONL 鏡像與資料庫一致、關機後 `kairos hosts list --json` 中主機為離線且帶主機資訊、重新啟動時從清冊載回主機。新增 durable 的端對端測試：Kairos 停機期間的取樣補送後不缺號、不重複；結果不明的重送與 Kairos 重新啟動後的重送都不重複。新增 `kairos reports import` 的端對端測試：Kairos 執行中拒絕（結束代碼 11，跨行程的檔案鎖）、停止後匯入與重複匯入、重新啟動後匯出與清冊。

### 發佈與建置

- 版本：`kairos`、`pulse` 改為 0.3.0；發佈包為 `kairos-0.3.0-<commit>-linux-x86_64.tar.gz`、`pulse-0.3.0-<commit>-linux-x86_64.tar.gz`。
- 新增依賴 `rusqlite`（MIT，只啟用 `bundled`：編譯內附的 SQLite 原始碼，不依賴系統的 libsqlite3，musl 靜態連結與 Windows 都不需額外安裝）及其依賴 `libsqlite3-sys`（MIT）、`fallible-iterator`、`fallible-streaming-iterator`（MIT／Apache-2.0）、`pkg-config`、`vcpkg`（建置用，MIT／Apache-2.0）。SQLite 本身是 public domain（不需授權條款），`THIRD-PARTY-LICENSES` 另加註說明。`deny.toml`、`packaging/about.toml` 的授權允許清單不需變更（沒有新的授權種類；未啟用 rusqlite 的 `cache` feature，避免引入 Zlib 授權的 `foldhash`）。
- Kairos 發佈包新增 `STORAGE.md`；Pulse 發佈包的 `pulse.service` 允許寫入 `buffer/`。

## [0.2.0] - 未單獨發佈（內容隨 0.8.0 發佈）

**Kairos 0.2.0**、**Pulse 0.2.0**（兩者同時改版）。共用 crate（`kairos-proto`、`date-guard`）是內部 crate，維持 0.1.0、不單獨發佈。協定新增 v2，與 v1 自動協商（見上方「版本相容矩陣」）。

### 破壞性變更與升級步驟（升級前必讀）

| # | 產品 | 變更 | 誰會受影響 | 升級步驟 |
|---|---|---|---|---|
| 1 | Kairos | **未啟用 mTLS 的對外監聽改為拒絕啟動**（S1） | `server.listen` 為非 loopback 位址（`0.0.0.0`、`::`、對外 IP，或解析成這些位址的主機名稱）而沒有設定 `auth.client_ca` 的部署 | 升級**前**依 `kairos/README.md`「啟用 mTLS」設定 `auth.client_ca`（建議同時設定 `auth.crl`）。只有隔離的測試環境才在 `kairos.toml` 的 `[auth]` 加上 `allow_unauthenticated = true`（或 `KAIROS__AUTH__ALLOW_UNAUTHENTICATED=true`）。只監聽 `127.0.0.1`／`::1` 的設定不受影響 |
| 2 | Pulse | **設定 `ca_cert` 時預設不再信任作業系統信任庫**（S5） | 設定了 `server.ca_cert`、但 Kairos 的伺服器憑證其實由公開 CA 簽發，又沒有明確設定 `server.use_system_roots` 的部署 | 依 INSTALL 部署（私有 CA）者不受影響。其餘請在 `pulse.toml` 的 `[server]` 加上 `use_system_roots = true`。升級後啟動日誌的「伺服器憑證信任來源：…」一行會寫出實際採用的信任來源；設定錯誤時會以「伺服器憑證不受信任」無法連線 |
| 3 | Pulse（影響 Kairos 的離線判定） | **心跳預設由 1 秒改為 15 秒**（A11） | 未自行設定 `heartbeat.interval` 的 Pulse | Kairos 的離線判定 `max(5 × 心跳, 5 秒)` 由 **5 秒變成 75 秒**：主機斷線後約 75 秒才會在 Kairos 顯示為離線；Pulse 的讀取閒置逾時由 11 秒變成 30 秒。依賴快速離線判定的監控或告警請調整門檻，或在 `pulse.toml` 設定 `[heartbeat] interval = 1`（1～60 秒）恢復舊行為。Kairos 不需修改 |
| 4 | Kairos | **既有資料檔與日誌檔的權限被收緊到最多 0640**（S3，Unix） | 以 Kairos 服務帳號以外、又不在其群組中的帳號讀取 `data/reports-*.jsonl`、`agents.json`、`logs/kairos-*.log` 的工具（例如日誌收集代理、備份腳本） | Kairos 開啟既有的 report 檔與日誌檔時會移除 group 寫入與 other 的所有權限（`0644` → `0640`，`0600` 維持不變），`agents.json` 與 `.retention-guard*.json` 每次重寫都是 `0640`。需要讀取的帳號請加入 Kairos 的群組（例如 `usermod -aG kairos <帳號>`）或以 ACL（`setfacl`）授權。既有目錄的權限不會被改變，但其他使用者可存取時（例如 `0755`）啟動會記 `WARN` 並建議 `chmod 750` |
| 5 | Kairos、Pulse | **啟動失敗的結束代碼依原因區分**（用法錯誤 2、設定錯誤 3、憑證錯誤 4，Kairos 另有連接埠衝突 11），systemd 不再重試 2、3、4 | 以結束代碼判斷啟動失敗的腳本或監控；沿用舊版 unit 檔的部署 | 依設計包 `design/design-system/cli-style.md` §8 的結束代碼表（不使用 sysexits）：用法錯誤 **2**（Kairos 原為 1，Pulse 不變）；設定錯誤 **3**（原為 1：設定檔無法載入或驗證不通過；Kairos 另含第 1 項的拒絕啟動，Pulse 另含無效的 `server.domain`）；憑證錯誤 **4**（原為 1：Kairos 的伺服器憑證、私鑰、client CA、CRL，Pulse 的 `ca_cert`、客戶端憑證、私鑰、SAN 不符與 `agent.id`）；Kairos 連接埠已被佔用 **11**（原為 1，仍會重試）。對照表見根目錄 README「結束代碼」。新版 `kairos.service`、`pulse.service` 加上 `RestartPreventExitStatus=2 3 4`：服務停在 `failed`（`status=2`／`3`／`4`），修正後執行 `systemctl restart`。升級時請以發佈包的 unit 覆寫 `/etc/systemd/system/` 下的檔案（自訂內容放在 `systemctl edit` 的 drop-in）並 `systemctl daemon-reload`；Pulse 多實例時依 `pulse/packaging/INSTALL.md` 第 9 節重新產生每個 `pulse-N.service`。只判斷「非 0」的腳本不受影響 |
| 6 | Kairos（影響讀取資料檔的工具） | **JSONL 每行新增 `schema`；新版 Pulse 連新版 Kairos 時 report 改為通用指標（`schema` 2）**（A3、A4、O6） | 直接讀取 `data/reports-*.jsonl` 或 `agents.json` 的工具（匯入、報表、監控腳本） | 每一行最前面多了 `schema`（`1` 為原本的 `cpu_usage`／`total_memory`／`used_memory`，`2` 為 `metrics` 陣列，記憶體等改以 bytes 表示）。兩端都升級後新資料為 `schema` 2，混用期間同一個檔案中兩種格式並存：升級 Kairos **前**請讓讀取工具依 `schema` 分流（沒有 `schema` 的舊資料視同 1，不認得的 `schema` 略過）。`agents.json` 每個 agent 多了 `proto` 與 v2 的主機資訊，`last_report` 的三個值在 v2 由標準指標換算（缺少時省略），另有 `metrics` 指標數。格式說明見 `kairos/README.md`「資料輸出」 |
| 7 | Kairos | **連線被拒與全域上限的日誌文字改寫**（A7） | 以日誌全文或固定字串比對來告警的監控（例如 grep「已達連線上限」） | 每 IP 上限：「來自 <位址> 的同時連線數已達上限 N（server.max_connections_per_ip），拒絕新連線」→「來自 <位址> 的同時連線數／握手中（尚未通過 hello）的連線數已達上限 N（<適用的設定>），在 TLS 握手前拒絕新連線」，並附允許清單的提示；全域上限：「已達連線上限 N，暫停接受新連線」→「已達同時連線上限 N（server.max_connections），暫停接受新連線；…」，恢復時另附累計暫停次數與時間；握手中上限的訊息加上「（server.max_connections 的一半）」與「在 TLS 握手前」。請改以「已達同時連線上限」「已達上限」等新字串或日誌等級比對。另外 agent 上線的 `INFO` 日誌結尾多了「，協定 vN」（比對前綴者不受影響） |
| 8 | Kairos、Pulse | **發佈包檔名加入版本，輸出位置與 `SHA256SUMS` 改變**（B3、D2） | 依檔名下載、解壓或比對雜湊的自動化腳本 | 檔名 `kairos-<commit>-linux-x86_64.tar.gz` → `kairos-<版本>-<commit>-linux-x86_64.tar.gz`（Pulse 相同），解壓後的目錄名稱也隨之改變；`--version` 由 `0.1.0 (<commit>)` 變為 `0.2.0 (<commit>)`。發佈處的 `SHA256SUMS` 改為兩個產品共用的一份並附 `SHA256SUMS.sig`，驗證步驟改為先以 `ssh-keygen -Y verify` 驗簽章，再 `sha256sum -c --ignore-missing`（INSTALL 第 2 節） |

**建議的升級順序**（協定 v2 以版本協商相容 v1，Kairos 與 Pulse 不必同時升級，以下順序是建議而非必要，理由見上方「版本相容矩陣」）：

1. 檢查 Kairos 是否符合第 1 項（`server.listen` 非 loopback 卻沒有 `auth.client_ca`），先啟用 mTLS。
2. 檢查第 4 項：有其他帳號讀取 Kairos 資料檔或日誌時，先把它加入 Kairos 的群組；第 6 項：讀取 JSONL 的工具先改為依 `schema` 分流；第 7 項：更新比對日誌文字的監控；第 8 項：更新下載與驗證發佈包的腳本。
3. 升級 Pulse（第 2、3 項；換上新版 `pulse.service`），確認 `systemctl status pulse` 的 `Status:` 為「已註冊：…」、日誌的信任來源正確。連舊版 Kairos 時註冊日誌為「協定 v1」。
4. 升級 Kairos（換上新版 `kairos.service`）。之後 Pulse 重連時註冊日誌變為「協定 v2」，JSONL 開始出現 `schema` 2 的行；尚未升級的 Pulse 繼續以 v1 運作。
5. 調整依賴離線判定時間（第 3 項）與結束代碼（第 5 項）的監控與腳本。

另有一項不需修改設定的行為改變：**啟用 mTLS 時 `server.max_connections_per_ip` 只計算尚未通過 hello 的連線**，通過後改依憑證身分計數（A7，見 Kairos「變更」）；同一個 NAT 後的 agent 不再從第 33 台起被斷線。

兩個產品之間的協定新增 **v2**，並以版本協商相容 v1（見「共用」與上方「版本相容矩陣」）：新版與舊版混用時自動以 v1 通訊、線上格式與舊版相同；兩端都是新版時才使用 v2（通用指標、主機資訊、`server_time`）。新舊版本混用的其他限制見各產品 README 的「升級注意事項（與 … 混用版本）」。

### Kairos

#### 新增

- `auth.allow_unauthenticated`（預設 `false`）：明確允許未啟用 mTLS 時監聽非 loopback 位址，只用於隔離的測試環境；允許時記 `WARN`，啟用 mTLS 時不生效（S1）。
- 伺服器憑證與 CRL 的到期預警（O1）：啟動時、熱重載套用新設定時與之後每個重載週期（停用熱重載時每小時）讀取伺服器憑證的 `notAfter` 與每份 CRL 的 `nextUpdate`，預警期間內記 `WARN`、已過期記 `ERROR`、恢復時記 `INFO`；每個檔案各自節流（狀態改變時立即記錄，其餘每 24 小時最多一次）。新設定 `server.cert_expiry_warning_days`（預設 30）、`auth.crl_expiry_warning_days`（預設 7），範圍 0～365，0 為不預警。CRL 過期時仍由 rustls 拒絕所有客戶端憑證（fail closed），行為不變。
- 發佈包新增 `kairos-crl.service` 與每週執行的 `kairos-crl.timer` 範例：產生 CRL → 以 CA 驗簽並確認 `nextUpdate` 在未來 → 原子改名 → 部署，任一步失敗即停止（INSTALL 第 10.3 節）。
- Unix 上伺服器私鑰（`server.key_path`）權限過寬時，啟動與熱重載換了私鑰時記 `WARN` 並附 `chown root:kairos`／`chmod 640` 的建議，不拒絕啟動（S8）。
- `kairos.service` 新增 `RestartPreventExitStatus=2 3 4`（用法、設定、憑證錯誤不重試）。
- 每 IP 上限的網段允許清單（A7 過渡方案，設計包 D14、D15、D22）：新設定 `server.per_ip_allowlist`（CIDR 清單，IPv4／IPv6，只寫位址表示單一位址；環境變數以逗號分隔）與 `server.max_connections_per_allowlisted_ip`（預設 4096，範圍 1～65535）。清單內的位址改用實際上限 min(`server.max_connections_per_allowlisted_ip`, `server.max_connections`)，新連線可連續的條數隨之為 `max(32, 2 × 實際上限)`，未啟用 mTLS 時以來源 IP 計算的 report 額度也以此為倍數。網段格式錯誤（含主機位元不為 0）時拒絕啟動並寫出第幾項與原因；清單非空且前者大於 `server.max_connections` 時啟動記一次 `WARN`（設計包 `KRS-W0203` 的情境），清單為空時不檢查、不警告。
- 被拒連線的計數（供日後 S20 與 `/metrics`）：在記憶體中分「因每 IP 上限」（依來源位址，附上限與是否在允許清單內；新連線速率另依來源計數）與「因全域上限」（暫停接受新連線的次數與累計時間、握手中上限拒絕的次數）累計，提供內部的快照 API；恢復接受新連線的日誌附上累計暫停時間，關機時記一行摘要。
- 啟動時以 `INFO` 記錄每 IP 上限的計數方式（是否只計算握手中的連線）、實際上限與允許清單。
- 協定 v2 與版本協商（A3）：依 hello 的 `proto`／`proto_min`／`proto_max` 協商（支援 1～2），`hello_ack` 回協商結果；v2 另回 `features`（`["metrics"]`）與 `server_time`（RFC 3339 UTC 毫秒，設計包 §12 #4）。舊版 Pulse 協商為 v1，`hello_ack`、report 格式與錯誤處理與舊版完全相同；協商為 v1 時 hello 中的 v2 主機資訊一律忽略（不檢查型別），與舊版 Kairos 相同。
- 通用指標（A4）：v2 且雙方都宣告 `metrics` 時，report 為 `metrics: [{name, value, unit, labels}]`，驗證名稱、單位、label、數量（1～256）、重複與標準指標的單位與範圍（違反時 `invalid_report`），原樣保存到 JSONL（`schema` 2，數字保留原本的表示）。
- v2 hello 的主機資訊（設計包 `enrollment-ux.md` §12 #5、`screens.md` §24 #15）：`host_fingerprint`、`display_name`、`tags`、`report_interval_ms`、`os` 依 PROTOCOL.md 的限制驗證（違反時 `bad_request`），寫入 `agents.json`；每個 agent 另記錄協商出的 `proto`。
- 同一個 `agent_id` 的主機指紋與狀態表中記錄的不同時記 `WARN`「主機指紋與上次不同」（每個 agent 節流），用於發現由映像檔複製、共用 `agent.id` 的主機。

#### 變更

- **（破壞性）** 未啟用 mTLS 又監聽非 loopback 位址時拒絕啟動（S1，見上表第 1 項）。
- **（破壞性）** Unix 上 report JSONL、`agents.json`、`.retention-guard*.json` 與日誌檔以 `0640` 建立，既有的 report 檔與日誌檔開啟時收緊到最多 `0640`；自行建立的資料目錄與日誌目錄（含執行中被刪除後重建）為 `0750`；既有目錄對其他使用者開放時記 `WARN`（S3，見上表第 4 項）。`agents.json` 與日期保護狀態檔的暫存檔建立時就是 `0640`，沒有暫存期間權限過寬的空窗。Windows 沒有對應的 mode，README「檔案權限」說明以 `icacls` 設定目錄 ACL。
- **（破壞性）** 啟動失敗的結束代碼依原因區分（見上表第 5 項）：命令列參數錯誤 1 → 2、設定錯誤 1 → 3、伺服器憑證／私鑰／client CA／CRL 載入失敗 1 → 4、`server.listen` 的連接埠已被佔用 1 → 11。背景工作異常、看門狗強制結束與其他啟動失敗（例如資料目錄、日誌目錄無法建立）仍為 1，第二次訊號強制結束仍為 130。
- 日誌依 UTC 日期分檔改由 Kairos 自行實作（`DailyLogFile`，取代 fern 的 `date-based`，以便指定新檔案的權限），檔名與保留規則不變；只在一筆日誌的開頭檢查日期，同一行不會被拆到兩個檔案。
- 協定型別、驗證與日期保護改用共用 crate（見「共用」），線上行為與日誌內容不變；`cpu_usage` 在線上統一為 f64、保存仍為 f32，接受範圍與 JSONL 輸出不變（A14）。
- **（行為改變）** 啟用 mTLS 時改依憑證身分計數（A7 根本解，設計包 D13）：`server.max_connections_per_ip` 只計算握手前、握手中與等待 hello 的連線（慢速握手防護，K-S2 仍有效）；hello 通過、`agent_id` 與憑證綁定後歸還來源位址的名額，改依 `agent_id` 計數，同一身分的多條連線依既有的 `superseded` 行為處理。同一個 NAT 後的 agent 數只受 `server.max_connections` 限制。設定鍵與預設值不變；為了 NAT 而調高 `server.max_connections_per_ip` 的 mTLS 部署可改回預設。未啟用 mTLS 時行為不變（每 IP 上限涵蓋整個連線）。
- **（破壞性）** JSONL 每行新增 `schema`（O6）：`1` 為三欄位的 report，`2` 為通用指標；新版 Pulse 連線時寫成 `schema` 2（見上表第 6 項）。
- **（破壞性）** 連線被拒的日誌改寫（見上表第 7 項）：每 IP 上限寫出計算的是「同時連線數」還是「握手中的連線數」、適用的設定（`server.max_connections_per_ip` 或允許清單），並提示可加入 `server.per_ip_allowlist`；全域上限與握手中上限的日誌寫出對應的設定（`server.max_connections`）。依賴舊日誌全文比對的監控請更新。
- `unsupported_version` 的 `message` 改為伺服器支援的範圍 `supported proto: 1-2`，並另帶 `proto_min`、`proto_max`（舊版 Pulse 忽略多出的欄位）；只送 `proto: 2`（沒有範圍）的 client 改以 v2 接受（舊版回 `unsupported_version`）。
- agent 上線的 `INFO` 日誌結尾加上「，協定 vN」；版本不受支援的 `WARN` 附上 client 宣告的範圍與本伺服器支援的範圍。

#### 修正

- Linux 上資料目錄被換成同名檔案等情況下，report 檔路徑查詢失敗（例如 `NotADirectory`）時沒有重新開檔，寫進已刪除的檔案卻仍回 `ack`。現在以路徑查詢失敗時一律重開（重開失敗則回 `storage_unavailable`），Unix 上另比對裝置與 inode，偵測檔案被替換。
- 命令列參數含非 UTF-8 字元時 panic（Q5）：改用 `args_os`，`--config <path>` 可接受非 UTF-8 路徑；其他無法辨識的參數印出用法後結束。

#### 文件

- README 不再寫死測試數量；修正 `agents.json` 暫存檔名、快速開始的啟動輸出、CI 的 `cargo deny check` 等與程式不一致的說明（Q8）。
- 心跳、離線判定的範例數值依 Pulse 的新預設（15 秒、75000 ms）更新；私有 CA 一節說明新版 Pulse 設定 `ca_cert` 時已預設只信任私有 CA。
- INSTALL 的權限總覽補上 Kairos 產生的檔案、私鑰權限的理由與確認清單，新增 CRL 定期更新（10.3）與 unit 的 `RestartPreventExitStatus`。

### Pulse

#### 新增

- 敏感檔案的權限檢查（S4）：`server.client_key` 與 `agent.id` 權限過寬時記 `WARN`（不阻止啟動、不自動修改）。Unix：其他使用者有任何權限、群組有寫入或執行權限時警告，群組可讀只在擁有者為 root 時允許（`root:pulse 640`）。Windows：讀取 DACL，Everyone、Authenticated Users、BUILTIN\Users 等一般使用者群組有讀取或寫入的允許項目（或 NULL DACL）時警告；不計算拒絕項目與有效權限（可能多報）。`--print-agent-id` 的 stderr 警告也涵蓋 Windows。
- 啟動時與斷線後第一次重連加上 0～min(心跳, 5 秒) 的隨機延遲，Kairos 重新啟動後大量 agent 不會在同一秒內重連（A11）。啟動延遲期間收到結束要求立即結束。
- 系統資訊取樣的逾時（Q4）：每次刷新逾時為 clamp(`report.interval`, 2 秒, 10 秒)，卡住時記 `WARN` 並略過該次 report（heartbeat 照常），期間不再發起新的刷新，每 10 分鐘提醒一次；恢復時記 `INFO` 並重建 CPU 基準。
- 啟動後立即以 sd_notify 送出 `Status:`「連線中」（O2），`systemctl status pulse` 在第一次註冊前也有狀態行。
- `pulse.service` 新增 `RestartPreventExitStatus=2 3 4`（用法、設定、憑證錯誤不重試）。
- 日誌初始化之後才發生、使 Pulse 以非 0 結束的錯誤（例如憑證錯誤），除了寫日誌也另寫一行 `[ERROR] …` 到 stderr；`pulse.service` 的 stdout 不進 journal，`journalctl -u pulse` 因此看得到服務停在 `failed` 的原因。
- 協定 v2 與版本協商（A3）：hello 送 `"proto":1`（舊版 Kairos 只接受 1）加上 `proto_min: 1`、`proto_max: 2`、`features: ["metrics"]`，依 `hello_ack` 的 `proto` 與 `features` 選擇 report 格式；連舊版 Kairos 時照舊送三欄位。註冊成功的日誌加上「協定 vN」。
- 通用指標（A4）：Kairos 支援時，report 改送 `system.cpu.usage`、`system.memory.total`／`used`（bytes），以及原本只在啟動時印在本機的磁碟容量（`system.filesystem.total`／`available`）、網路累計流量（`system.network.received`／`transmitted`）與元件溫度（`system.temperature`）。篩選規則同啟動時的 INFO 清單，各類最多 32 個；整行超過 65536 bytes 時捨去磁碟／網路／溫度指標。
- v2 hello 的主機資訊：`host_fingerprint`（Linux `/etc/machine-id`、Windows `MachineGuid` 加上用途字串後的 SHA-256，不送原始值；使用 rustls 已內建的 ring，不新增依賴）、`report_interval_ms`、`os`（sysinfo 的作業系統名稱、版本、核心版本、CPU 架構），以及新設定 `agent.display_name`、`agent.tags`（預設不送；不符合 Kairos 的限制時以結束代碼 3 拒絕啟動）。
- v2 的 `hello_ack` 帶 `server_time`：與本機時鐘相差超過 5 分鐘時記 `WARN`（設計包 PF03）。

#### 變更

- **（破壞性）** `server.use_system_roots` 預設改為依 `ca_cert` 決定：設定了 `ca_cert` 時只信任 `ca_cert`，未設定時信任作業系統信任庫；明確設定時以設定為準（S5，見上表第 2 項）。啟動時以 `INFO` 記錄實際採用的信任來源。
- **（破壞性）** `heartbeat.interval` 預設由 1 秒改為 15 秒（A11，見上表第 3 項）。
- **（破壞性）** 設定錯誤（`pulse.toml`／環境變數的語法、型別、範圍錯誤，`log.level` 無效，`use_system_roots = false` 卻未設定 `ca_cert`）的結束代碼由 1 改為 3；憑證錯誤（`ca_cert`、客戶端憑證或私鑰無法讀取、格式錯誤、不成對，SAN 與 agent_id 不符，`agent.id` 無法讀取、內容不合法或無法建立）由 1 改為 4（依設計包 `design/pulse/cli-spec.md` §1.3，見上表第 5 項）。用法錯誤仍為 2，日誌目錄無法建立等其他啟動失敗與內部錯誤仍為 1。
- `server.domain` 不是有效的 DNS 名稱或 IP 位址時，在載入設定時就以設定錯誤（3）結束，而不是等到建立 TLS 設定時才失敗。
- 第一筆 report 的 CPU 使用率改為涵蓋一個完整的 `report.interval`（Q3）：啟動時先建立基準，第一次取樣在一個 `report.interval` 之後，因此啟動後要等一個 `report.interval` 才有第一筆 report。
- 連續失敗達 `retry.max_retries` 後的 `ERROR` 日誌每 10 分鐘最多一則，其餘改記 `WARN`，下一則 `ERROR` 附上次數；連線穩定 30 秒以上後重新起算（Q6）。
- 伺服器送來不認得的訊息類型時改記 `DEBUG`（獨立節流），壞掉的訊息仍記 `WARN`；兩者都照舊忽略（A14）。
- 協定型別、驗證與日誌日期保護改用共用 crate（見「共用」），線上行為與日誌內容不變。
- `hello_ack` 的 `proto` 不在 1～2 時才視為版本不符（`proto_mismatch`，原為不等於 1），提示文字改為支援的範圍。

#### 文件

- INSTALL：`client.key`、`agent.id` 改為 `root:pulse 640`（服務帳號唯讀），附舊安裝的遷移指令；舊配置 `pulse:pulse 600` 仍可運作、不會警告（S8）。
- README 不再寫死測試數量，並新增測試防止再寫回去（Q8）；核對 sd_notify 的 `Status:` 文字與 unit（O2）。
- README 的「本版的行為改變」併入本檔案。

### 共用

#### 新增

- `crates/kairos-proto`（`publish = false`）：協定常數、`ClientMsg`／`ServerMsg`／`ErrorCode`、訊息解析、hello／ts／report 欄位驗證、禁止字元表、UUID 格式與 `agent_id` 正規化，Kairos 與 Pulse 共用一份實作（A8）。
- `tests/e2e`（`kp-e2e`，`publish = false`）：端對端測試，以實際編譯出的 `kairos`、`pulse` 執行檔在 127.0.0.1 的隨機埠上以 mTLS 互連（Q1）：註冊（協商為協定 v2）、heartbeat、report 寫入 JSONL（`schema` 2）、CRL 熱重載撤銷、Kairos 重啟後 Pulse 自動重連、兩端優雅關機、協定 v1 相容（以送出舊版 Pulse 訊息的假 client 與新版 Pulse 同時連線），以及兩端啟動時的結束代碼。只用標準函式庫與 workspace 已使用的 `serde_json`、`rustls`，不連外部網路；Linux 與 Windows 都會執行，整套約 10 秒。說明見根目錄 README「端對端測試」。CI 先建置兩個執行檔，再以獨立步驟執行端對端測試。
- `crates/date-guard`（`publish = false`）：依日期命名檔案的保留清除規則與時鐘跳躍／回撥保護（DateGuard），Kairos 的 report 檔與日誌、Pulse 的日誌共用（A8）。狀態檔格式不變，Kairos 仍接受舊版格式。
- **協定 v2**（A3、A4）：`PROTOCOL.md` 新增「版本協商」（規則、features、相容矩陣、舊端忽略新欄位的行為）、v2 hello 欄位限制、「主機指紋」的計算方式與測試向量、「report（通用指標）」的格式、上限與標準指標表，「範例」改為 v1、v2、錯誤三個小節。`kairos-proto` 新增 `negotiate`（`client_proto_range`、`negotiate`、`Session`）、`metrics`（`Metric`、`validate_metrics`）、`HelloExt`／`HostInfo`／`ReportBody` 與對應的驗證；v2 欄位解析失敗不會讓整則訊息失敗，協商為 v1 時與 v1 實作的行為相同。測試從規格解析 v1／v2 向量、上限、標準指標表與主機指紋範例，並以凍結的 v1 型別定義驗證新舊混用。

#### 變更

- 協定規格只留一份：`crates/kairos-proto/PROTOCOL.md`。`kairos/PROTOCOL.md`、`pulse/PROTOCOL.md` 改為指向它的說明，發佈包內附的仍是完整規格。協定測試以 `include_str!` 直接從規格的「範例」節解析測試向量、比對「錯誤碼」表，取代兩個產品各自寫死的複本。
- 規格中 Pulse 讀取閒置的範例數值依新的預設心跳更新（15 秒時為 30 秒）。

### 發佈與建置

#### 新增

- **版本號**：Kairos、Pulse 改為 0.2.0 並採用 SemVer，`--version` 為 `<名稱> 0.2.0 (<commit>)`；共用 crate 維持 0.1.0（內部 crate，不單獨發佈）。版本規則、打 tag 與發佈流程見 `RELEASING.md`（B3）。
- **簽章**：發佈處提供 `SHA256SUMS.sig`（`ssh-keygen -Y sign`，namespace `kairos-pulse-release`）。INSTALL 第 2 節的驗證改為先以 `ssh-keygen -Y verify` 驗簽章、再 `sha256sum -c --ignore-missing`；發佈公鑰 `allowed_signers` 須從另一個管道取得並核對指紋（D2）。
- **第三方授權聲明**：發佈包新增 `THIRD-PARTY-LICENSES`（cargo-about 依 `Cargo.lock` 產生，含各依賴的授權全文），INSTALL 的安裝與升級步驟一併把它與 `CHANGELOG.md` 複製到 `/opt/<產品>/`（B2）。
- **SBOM**：發佈包新增 `<名稱>.cdx.json`（CycloneDX 1.5，cargo-cyclonedx 產生），發佈處另附一份 `<名稱>-<版本>-<commit>-linux-x86_64.cdx.json`（D4）。
- `packaging/release.sh`：檢查工作目錄乾淨、執行 fmt／clippy／test（與 CI 相同，端對端測試分開執行）／deny 後打包兩個產品，提供私鑰時簽章；`packaging/sign-release.sh`：簽章與驗證；`packaging/check-reproducible.sh`：以兩個全新的建置環境各建置一次並比對雜湊（D3、D4）。
- CI 新增 `package` job：Linux 與 Windows 的 fmt、clippy、test 與 cargo deny 都通過後才打包，產出兩個發佈包、SBOM 與合併版 `SHA256SUMS`（artifact，不簽章）；push 到 `main` 與手動觸發時另跑可重現性檢查（D3、D4）。

#### 變更

- **（破壞性）** **發佈包檔名加入版本**：`kairos-<commit>-linux-x86_64.tar.gz` → `kairos-<版本>-<commit>-linux-x86_64.tar.gz`（Pulse 相同），依檔名下載或解壓的自動化腳本需要調整（見上表第 8 項）。
- 兩個產品的打包腳本都輸出到 repo 根目錄的 `dist/`，`SHA256SUMS` 改為兩個產品共用的合併清單（修正每次打包都覆寫成只剩單一套件的問題，D2）。
- 建置 image 以 `@sha256:` digest 固定，唯一需要 `apk add` 的 GNU tar 鎖定版本（D4）。
- 建置來源改用 `git archive` 匯出的 commit 內容，不再掛載工作目錄：Windows（`core.autocrlf=true`）上打出的發佈包中文件與範例設定原本是 CRLF，Pulse 嵌入的預設設定也因此與 Linux 上建置的不同；現在與主機的行尾設定無關。
