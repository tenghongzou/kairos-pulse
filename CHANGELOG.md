# 變更紀錄

本檔案記錄 Kairos、Pulse 與共用 crate 值得注意的變更，格式依 [Keep a Changelog](https://keepachangelog.com/zh-TW/1.1.0/)。兩個產品尚未有正式版號，發佈包以 commit 識別（`kairos-<commit>-linux-x86_64.tar.gz`、`pulse-<commit>-linux-x86_64.tar.gz`），因此目前只有 `[Unreleased]` 區段。

各項後的代號（例如 S1、A11）對應商業審計的項目編號。

## [Unreleased]

### 破壞性變更與升級步驟（升級前必讀）

| # | 產品 | 變更 | 誰會受影響 | 升級步驟 |
|---|---|---|---|---|
| 1 | Kairos | **未啟用 mTLS 的對外監聽改為拒絕啟動**（S1） | `server.listen` 為非 loopback 位址（`0.0.0.0`、`::`、對外 IP，或解析成這些位址的主機名稱）而沒有設定 `auth.client_ca` 的部署 | 升級**前**依 `kairos/README.md`「啟用 mTLS」設定 `auth.client_ca`（建議同時設定 `auth.crl`）。只有隔離的測試環境才在 `kairos.toml` 的 `[auth]` 加上 `allow_unauthenticated = true`（或 `KAIROS__AUTH__ALLOW_UNAUTHENTICATED=true`）。只監聽 `127.0.0.1`／`::1` 的設定不受影響 |
| 2 | Pulse | **設定 `ca_cert` 時預設不再信任作業系統信任庫**（S5） | 設定了 `server.ca_cert`、但 Kairos 的伺服器憑證其實由公開 CA 簽發，又沒有明確設定 `server.use_system_roots` 的部署 | 依 INSTALL 部署（私有 CA）者不受影響。其餘請在 `pulse.toml` 的 `[server]` 加上 `use_system_roots = true`。升級後啟動日誌的「伺服器憑證信任來源：…」一行會寫出實際採用的信任來源；設定錯誤時會以「伺服器憑證不受信任」無法連線 |
| 3 | Pulse（影響 Kairos 的離線判定） | **心跳預設由 1 秒改為 15 秒**（A11） | 未自行設定 `heartbeat.interval` 的 Pulse | Kairos 的離線判定 `max(5 × 心跳, 5 秒)` 由 **5 秒變成 75 秒**：主機斷線後約 75 秒才會在 Kairos 顯示為離線；Pulse 的讀取閒置逾時由 11 秒變成 30 秒。依賴快速離線判定的監控或告警請調整門檻，或在 `pulse.toml` 設定 `[heartbeat] interval = 1`（1～60 秒）恢復舊行為。Kairos 不需修改 |
| 4 | Kairos | **既有資料檔與日誌檔的權限被收緊到最多 0640**（S3，Unix） | 以 Kairos 服務帳號以外、又不在其群組中的帳號讀取 `data/reports-*.jsonl`、`agents.json`、`logs/kairos-*.log` 的工具（例如日誌收集代理、備份腳本） | Kairos 開啟既有的 report 檔與日誌檔時會移除 group 寫入與 other 的所有權限（`0644` → `0640`，`0600` 維持不變），`agents.json` 與 `.retention-guard*.json` 每次重寫都是 `0640`。需要讀取的帳號請加入 Kairos 的群組（例如 `usermod -aG kairos <帳號>`）或以 ACL（`setfacl`）授權。既有目錄的權限不會被改變，但其他使用者可存取時（例如 `0755`）啟動會記 `WARN` 並建議 `chmod 750` |
| 5 | Kairos、Pulse | **啟動失敗的結束代碼依原因區分**（用法錯誤 2、設定錯誤 3、憑證錯誤 4，Kairos 另有連接埠衝突 11），systemd 不再重試 2、3、4 | 以結束代碼判斷啟動失敗的腳本或監控；沿用舊版 unit 檔的部署 | 依設計包 `design/design-system/cli-style.md` §8 的結束代碼表（不使用 sysexits）：用法錯誤 **2**（Kairos 原為 1，Pulse 不變）；設定錯誤 **3**（原為 1：設定檔無法載入或驗證不通過；Kairos 另含第 1 項的拒絕啟動，Pulse 另含無效的 `server.domain`）；憑證錯誤 **4**（原為 1：Kairos 的伺服器憑證、私鑰、client CA、CRL，Pulse 的 `ca_cert`、客戶端憑證、私鑰、SAN 不符與 `agent.id`）；Kairos 連接埠已被佔用 **11**（原為 1，仍會重試）。對照表見根目錄 README「結束代碼」。新版 `kairos.service`、`pulse.service` 加上 `RestartPreventExitStatus=2 3 4`：服務停在 `failed`（`status=2`／`3`／`4`），修正後執行 `systemctl restart`。升級時請以發佈包的 unit 覆寫 `/etc/systemd/system/` 下的檔案（自訂內容放在 `systemctl edit` 的 drop-in）並 `systemctl daemon-reload`；Pulse 多實例時依 `pulse/packaging/INSTALL.md` 第 9 節重新產生每個 `pulse-N.service`。只判斷「非 0」的腳本不受影響 |
| 6 | Kairos（影響讀取資料檔的工具） | **JSONL 每行新增 `schema`；新版 Pulse 連新版 Kairos 時 report 改為通用指標（`schema` 2）**（A3、A4、O6） | 直接讀取 `data/reports-*.jsonl` 或 `agents.json` 的工具（匯入、報表、監控腳本） | 每一行最前面多了 `schema`（`1` 為原本的 `cpu_usage`／`total_memory`／`used_memory`，`2` 為 `metrics` 陣列，記憶體等改以 bytes 表示）。兩端都升級後新資料為 `schema` 2，混用期間同一個檔案中兩種格式並存：升級 Kairos **前**請讓讀取工具依 `schema` 分流（沒有 `schema` 的舊資料視同 1，不認得的 `schema` 略過）。`agents.json` 每個 agent 多了 `proto` 與 v2 的主機資訊，`last_report` 的三個值在 v2 由標準指標換算（缺少時省略），另有 `metrics` 指標數。格式說明見 `kairos/README.md`「資料輸出」 |

**建議的升級順序**：

1. 檢查 Kairos 是否符合第 1 項（`server.listen` 非 loopback 卻沒有 `auth.client_ca`），先啟用 mTLS。
2. 檢查第 4 項：有其他帳號讀取 Kairos 資料檔或日誌時，先把它加入 Kairos 的群組；第 6 項：讀取 JSONL 的工具先改為依 `schema` 分流。
3. 升級全部 Pulse（第 2、3 項；換上新版 `pulse.service`），確認 `systemctl status pulse` 的 `Status:` 為「已註冊：…」、日誌的信任來源正確。
4. 升級 Kairos（換上新版 `kairos.service`）。
5. 調整依賴離線判定時間（第 3 項）與結束代碼（第 5 項）的監控與腳本。

另有一項不需修改設定的行為改變：**啟用 mTLS 時 `server.max_connections_per_ip` 只計算尚未通過 hello 的連線**，通過後改依憑證身分計數（A7，見 Kairos「變更」）；同一個 NAT 後的 agent 不再從第 33 台起被斷線。

兩個產品之間的協定新增 **v2**，並以版本協商相容 v1（見「共用」）：新版與舊版混用時自動以 v1 通訊、線上格式與舊版相同，因此 Kairos 與 Pulse 不必同時升級，上面的升級順序（先 Pulse 後 Kairos）只是建議。兩端都是新版時才使用 v2（通用指標、主機資訊、`server_time`）。新舊版本混用的其他限制見各產品 README 的「升級注意事項（與 … 混用版本）」。

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
- JSONL 每行新增 `schema`（O6）：`1` 為三欄位的 report，`2` 為通用指標（見上表第 6 項）。
- v2 hello 的主機資訊（設計包 `enrollment-ux.md` §12 #5、`screens.md` §24 #15）：`host_fingerprint`、`display_name`、`tags`、`report_interval_ms`、`os` 依 PROTOCOL.md 的限制驗證（違反時 `bad_request`），寫入 `agents.json`；每個 agent 另記錄協商出的 `proto`。
- 同一個 `agent_id` 的主機指紋與狀態表中記錄的不同時記 `WARN`「主機指紋與上次不同」（每個 agent 節流），用於發現由映像檔複製、共用 `agent.id` 的主機。

#### 變更

- **（破壞性）** 未啟用 mTLS 又監聽非 loopback 位址時拒絕啟動（S1，見上表第 1 項）。
- **（破壞性）** Unix 上 report JSONL、`agents.json`、`.retention-guard*.json` 與日誌檔以 `0640` 建立，既有的 report 檔與日誌檔開啟時收緊到最多 `0640`；自行建立的資料目錄與日誌目錄（含執行中被刪除後重建）為 `0750`；既有目錄對其他使用者開放時記 `WARN`（S3，見上表第 4 項）。`agents.json` 與日期保護狀態檔的暫存檔建立時就是 `0640`，沒有暫存期間權限過寬的空窗。Windows 沒有對應的 mode，README「檔案權限」說明以 `icacls` 設定目錄 ACL。
- **（破壞性）** 啟動失敗的結束代碼依原因區分（見上表第 5 項）：命令列參數錯誤 1 → 2、設定錯誤 1 → 3、伺服器憑證／私鑰／client CA／CRL 載入失敗 1 → 4、`server.listen` 的連接埠已被佔用 1 → 11。背景工作異常、看門狗強制結束與其他啟動失敗（例如資料目錄、日誌目錄無法建立）仍為 1，第二次訊號強制結束仍為 130。
- 日誌依 UTC 日期分檔改由 Kairos 自行實作（`DailyLogFile`，取代 fern 的 `date-based`，以便指定新檔案的權限），檔名與保留規則不變；只在一筆日誌的開頭檢查日期，同一行不會被拆到兩個檔案。
- 協定型別、驗證與日期保護改用共用 crate（見「共用」），線上行為與日誌內容不變；`cpu_usage` 在線上統一為 f64、保存仍為 f32，接受範圍與 JSONL 輸出不變（A14）。
- **（行為改變）** 啟用 mTLS 時改依憑證身分計數（A7 根本解，設計包 D13）：`server.max_connections_per_ip` 只計算握手前、握手中與等待 hello 的連線（慢速握手防護，K-S2 仍有效）；hello 通過、`agent_id` 與憑證綁定後歸還來源位址的名額，改依 `agent_id` 計數，同一身分的多條連線依既有的 `superseded` 行為處理。同一個 NAT 後的 agent 數只受 `server.max_connections` 限制。設定鍵與預設值不變；為了 NAT 而調高 `server.max_connections_per_ip` 的 mTLS 部署可改回預設。未啟用 mTLS 時行為不變（每 IP 上限涵蓋整個連線）。
- 連線被拒的日誌改寫：每 IP 上限寫出計算的是「同時連線數」還是「握手中的連線數」、適用的設定（`server.max_connections_per_ip` 或允許清單），並提示可加入 `server.per_ip_allowlist`；全域上限與握手中上限的日誌寫出對應的設定（`server.max_connections`）。依賴舊日誌全文比對的監控請更新。
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
- `tests/e2e`（`kp-e2e`，`publish = false`）：端對端測試，以實際編譯出的 `kairos`、`pulse` 執行檔在 127.0.0.1 的隨機埠上以 mTLS 互連（Q1）：註冊、heartbeat、report 寫入 JSONL、CRL 熱重載撤銷、Kairos 重啟後 Pulse 自動重連、兩端優雅關機，以及兩端啟動時的結束代碼。只用標準函式庫與既有的 `serde_json`，不連外部網路；Linux 與 Windows 都會執行，整套約 10 秒。說明見根目錄 README「端對端測試」。CI 先建置兩個執行檔，再以獨立步驟執行端對端測試。
- `crates/date-guard`（`publish = false`）：依日期命名檔案的保留清除規則與時鐘跳躍／回撥保護（DateGuard），Kairos 的 report 檔與日誌、Pulse 的日誌共用（A8）。狀態檔格式不變，Kairos 仍接受舊版格式。
- **協定 v2**（A3、A4）：`PROTOCOL.md` 新增「版本協商」（規則、features、相容矩陣、舊端忽略新欄位的行為）、v2 hello 欄位限制、「主機指紋」的計算方式與測試向量、「report（通用指標）」的格式、上限與標準指標表，「範例」改為 v1、v2、錯誤三個小節。`kairos-proto` 新增 `negotiate`（`client_proto_range`、`negotiate`、`Session`）、`metrics`（`Metric`、`validate_metrics`）、`HelloExt`／`HostInfo`／`ReportBody` 與對應的驗證；v2 欄位解析失敗不會讓整則訊息失敗，協商為 v1 時與 v1 實作的行為相同。測試從規格解析 v1／v2 向量、上限、標準指標表與主機指紋範例，並以凍結的 v1 型別定義驗證新舊混用。

#### 變更

- 協定規格只留一份：`crates/kairos-proto/PROTOCOL.md`。`kairos/PROTOCOL.md`、`pulse/PROTOCOL.md` 改為指向它的說明，發佈包內附的仍是完整規格。協定測試以 `include_str!` 直接從規格的「範例」節解析測試向量、比對「錯誤碼」表，取代兩個產品各自寫死的複本。
- 規格中 Pulse 讀取閒置的範例數值依新的預設心跳更新（15 秒時為 30 秒）。
