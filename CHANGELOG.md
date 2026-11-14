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
- 0.9.0 的協定版本與 8444 的 API 都與 0.8.0 相同，8443 另以 feature `cluster`（節點名稱與關機排空）、`config`、`upgrade`（集中設定與遠端升級）協商（雙方都宣告時才使用），0.8 與 0.9 的任何組合都能連線（各組合見 [0.9.0] 的「相容性」）；Kairos 的資料庫升級到 schema v9（遠端管理的資料表）。
- 0.10.0 的協定版本、8443 的 feature 與 8444 的 API 都與 0.9.0 相同，0.9 與 0.10 的任何組合都能連線（各組合見 [0.10.0] 的「相容性」）；Kairos 的資料庫升級到 schema v11（v10：叢集協調的資料表；v11：升級推送的雙人覆核），另可改用 PostgreSQL 儲存後端與叢集模式（主動／備援，HA 預覽）。外部 URL 的版本庫需要 Pulse 0.10 與本機的 `upgrade.url_allowlist`，其餘 0.10 的遠端管理功能 Pulse 0.9 就能使用。
- 0.11.0 的協定版本、8443 的 feature 與 8444 的 API 都與 0.10 相同，0.9、0.10 與 0.11 的任何組合都能連線（hello 的 `install.external_url` 是選用欄位，見 [0.11.0] 的「相容性」）；Kairos 的資料庫升級到 schema v12（`hosts.install_external_url`），升級後 0.10.1 以前的 Kairos 無法開啟。
- 0.1.0 與 0.2.0 的任何組合都有共同的協定版本，不會出現 `unsupported_version`；它只會在日後某一端不再支援 v1（例如只支援 v3 以上）時出現。
- **升級順序**：協定上任何順序都可以，混用期間自動以 v1 通訊，不必同時升級。仍**建議先升級 Pulse、再升級 Kairos**，原因是上表「Kairos 0.2.0 × Pulse 0.1.0」的限制（舊版 Pulse 的主機名稱與讀取閒置問題）只在先升級 Kairos 時出現；反過來的組合只有第 3 項的離線判定時間改變。資料格式則在 Kairos 升級的那一刻改變（JSONL 開始有 `schema`），與 Pulse 的升級順序無關。從 0.2.0 升級到 0.3.0 時沒有這些限制，**建議先升級 Kairos、再升級 Pulse**（Pulse 升級後的第一次斷線就能補送），步驟見 [0.3.0]。
- 驗證方式：`kairos-proto` 以凍結的 v1 型別定義驗證舊端忽略 v2 欄位；Kairos 整合測試以 v1／v2 client 涵蓋各種協商結果；Pulse 單元測試以記憶體內的 v1／v2 伺服器驗證 report 格式的選擇；端對端測試（`tests/e2e`）以實際的執行檔驗證 0.2.0 × 0.2.0（v2），並以送出 0.1.0 Pulse 訊息的假 client 驗證 0.2.0 的 Kairos × 0.1.0 的 Pulse（v1）。沒有以 0.1.0 的執行檔實測。

## 支援期限

> 以下為待使用者決定的草稿，數字皆為建議值，尚未對外承諾。

| 項目 | 建議 |
|---|---|
| 支援的版本線 | 最新的 MINOR 版本線（目前 0.11.x）提供錯誤與安全性修正（PATCH） |
| 前一個版本線 | 新的 MINOR 版本發佈後〔待定：建議 6 個月〕內只提供安全性修正，之後終止支援 |
| PATCH 版本 | 同一 MINOR 版本線內不含破壞性變更，可直接升級 |
| Kairos／Pulse 混用 | 保證相鄰兩個 MINOR 版本線可以混用（例如 Kairos 0.3 搭配 Pulse 0.2），以便分批升級〔待定〕 |
| 終止支援通知 | 至少提前〔待定：建議 3 個月〕在本檔案與發佈公告中通知 |
| 平台 | Linux x86_64、aarch64（musl 靜態連結；aarch64 自 0.8.0 起）、systemd 247 以上；Pulse 另有 Windows x86_64（0.8.0 起，**未在 Windows 實機驗證前不列入支援**）；macOS 不提供 |
| 1.0 之後 | 另訂長期支援（LTS）政策〔待定〕 |

## [Unreleased]

### 安全修正

2026-11 安全稽核中評為「中」、延後到這一版的項目。影響的版本寫在各項。

- **註冊端點（8444）的輪詢與撤回補上每個來源的限速與封鎖檢查，限流表有筆數上限**（安全稽核 C1；Kairos；0.5.0 起受影響，前提是 `enroll.mode` 不是 `closed`）：輪詢（`GET /v1/enroll/requests/{id}`）與撤回的兩個請求原本只有每筆申請的限速（L10），沒有每個來源的限速，也不檢查 L7 的封鎖；被封鎖的來源繼續送無效的值時，計數表仍逐筆累積；限流表滿了之後每個新的鍵都在鎖內掃描整張表。結果是不需要任何憑據的來源可以讓 8444（以及共用工作執行緒的 8443）變慢，來源很多時佔用記憶體。這一版起：(1) 這三個端點先檢查 L7 封鎖（輪詢看新註冊類、撤回看續簽類；過期續簽申請的輪詢另看續簽類），再過新的每來源限速 **L4p**（沒有新的設定鍵：速率為 `enroll.poll_target_per_ip_per_sec` × 4、可連續的次數為速率 × 30 秒，預設每秒 20 次、可連續 600 次；批次部署來源乘以倍率），被拒的請求不讀資料庫、不計入 L7 與 L10；(2) 封鎖期間不再計數；(3) 每張限流表最多 262,144 個鍵，滿的時候新的鍵回 429、既有的鍵不受影響，L7 的計數表與封鎖表各最多 65,536 個來源、滿的時候成批淘汰最久沒有出現的來源；清理的成本攤在之後的請求上，不再每個請求掃描整張表。守規則的 Pulse 不受影響（動態 `poll_after_ms` 本來就把同一來源的輪詢壓在 `poll_target_per_ip_per_sec` 以下）；所有版本的 Pulse 都已經會處理輪詢時的 429 `rate_limited`／`source_blocked`（依 `Retry-After` 等候）。規格：`crates/kairos-proto/ENROLL.md` 第 4、6.4、8.2、11 節。
- **過期續簽檢查 `auth.crl`**（安全稽核 C2；Kairos；0.5.0～0.11.0 受影響，前提是以外部 CA 手動簽發用戶端憑證、以 `auth.crl` 撤銷、沒有把憑證匯入 Kairos）：8443 以 `auth.crl` 拒絕被撤銷的憑證，但 8444 的過期續簽只看 Kairos 自己的撤銷紀錄。只在外部 CRL 撤銷的憑證過期之後，持有舊私鑰的一方在寬限期內（預設 90 天）仍可換到一張新的憑證。這一版起過期續簽對 `auth.client_ca` 的 CA 簽出的舊憑證以同一組 `auth.crl` 檢查（整條鏈；CRL 的 `nextUpdate` 以現在的時間判定）：被撤銷 → 403 `certificate_revoked`；無法判定（CRL 讀不到、已超過 `nextUpdate`、沒有涵蓋簽發者）→ 與 8443 一樣不放行，500 `internal`，Pulse 依退避重試。沒有設定 `auth.crl`、或舊憑證由 Kairos 的簽發 CA 簽發時行為不變。`kairos/PKI.md` 第 9、13 節、ENROLL.md 第 7.2 節第 10 步。
- **主控台的一行安裝指令在執行安裝腳本之前先比對它的 SHA-256**（安全稽核 C3；Kairos 主控台；0.9.0～0.11.0 受影響，前提是設定了 `enroll.download_dir`）：指令從 8444 下載 `pulse-install.sh` 後直接以 root 執行。腳本會以發佈公鑰驗證套件，但腳本本身沒有經過任何驗證，文件「完整性不依賴 Kairos」的說法對腳本不成立；能改寫下載目錄的人可以讓之後照指令安裝的主機執行被改過的腳本。這一版起主控台 API 的 `downloads` 多 `install_script_sha256`，指令變成「下載 → `sha256sum -c` → 相符才 `sudo sh pulse-install.sh …`」；安裝腳本與同目錄 `SHA256SUMS` 記錄的雜湊不符（或不是一般檔案）時不提供一行指令（`KRS-W2012`，`downloads.reason` 說明原因）。文件更正為「套件的完整性不依賴 Kairos；腳本本身依賴主控台與下載目錄」，並補上下載目錄應只有管理者可寫（`kairos/PKI.md` 第 17 節、`pulse/packaging/INSTALL.md` 第 A.1 節、API.md）。這是降低風險而不是消除：雜湊由 Kairos 從下載目錄算出，Kairos 本身被入侵時仍可顯示任意指令。
- **稽核鏈的鏈尾與錨點指標受 HMAC 保護，刪尾與刪頭可以被驗證發現**（安全稽核 E1；Kairos；0.7.0～0.11.0 受影響，前提是能直接改寫資料庫但拿不到 `audit-hmac.key`，主要是 PostgreSQL 後端的資料庫管理者）：`meta.audit_chain_head` 的 `mac` 只涵蓋鏈尾，驗證不要求它存在；`meta.audit_chain_anchor` 沒有保護。刪掉最後幾筆並刪掉 head，或刪掉最前面幾筆並改寫錨點，`kairos audit verify`、API 與自我驗證都回報完整——文件「只刪除最後幾筆也會被發現」不成立。這一版起：head 改為第 2 版，`mac` 同時涵蓋鏈尾與錨點；HMAC 開始之後 head 必須存在；head 內的錨點必須與 `meta.audit_chain_anchor` 相同；寫入與保留期限清除在 head 與現況不一致時**不更新 head**（不會因為 Kairos 之後又寫了一筆就把痕跡蓋掉）。SQLite 與 PostgreSQL 都是；**沒有 schema 變更**（仍是 v12，只改 `meta` 裡這個值的格式）。斷點原因沿用 `head_mismatch`、`anchor_mismatch`（`KRS-E6201`、系統告警 `sys.audit_integrity`）。仍然的限制不變：刪除 `meta.audit_hmac` 與所有 `hmac` 的「降級」、以及把稽核資料整份換回舊的（重播），要靠外送的鏈尾雜湊在外部發現。`kairos/API.md` 第 12 節「head 與錨點的保護」。

### 升級注意

- **同一個來源位址後面有大量主機同時等待核准、而且那個來源被 L7 封鎖時**，封鎖期間這些主機的輪詢也會收到 429 `source_blocked`（以前只擋新的申請）；封鎖結束後自動恢復，已核准的憑證不會遺失。
- **有設定 `auth.crl` 的部署：CRL 過期時，過期續簽也會被拒**（C2）。以前 `auth.crl` 超過 `nextUpdate` 只影響 8443（握手失敗、`KRS-E1203`）；現在 `auth.client_ca` 的 CA 簽出的憑證在 8444 的過期續簽同樣不放行（Kairos 記一行警告，Pulse 自動重試）。照常維持 CRL 的更新（`kairos-crl.timer` 或你的 CA 流程）即可，不需要改設定。曾經只在外部 CRL 撤銷憑證的部署：升級後那些憑證不能再以過期續簽換新憑證；如果在 0.5.0～0.11.0 期間有被撤銷的憑證已經過期，請在憑證清單（S14）檢查那些主機是否有不該存在的「過期續簽」簽發紀錄，有的話撤銷。
- **使用一行安裝指令的部署**（C3）：升級後請從主控台重新複製指令（舊的指令仍可執行，但不比對腳本）；自動化若是自己組出這行指令，請加上比對（雜湊取自主控台 API 的 `downloads.install_script_sha256`，或以發佈公鑰驗證過的 `SHA256SUMS`）。確認 `enroll.download_dir` 與其中的檔案只有管理者可寫。下載目錄裡的 `pulse-install.sh` 與 `SHA256SUMS` 不是同一個發佈版本時，升級後主控台不再顯示一行指令，放入同一版的檔案即可恢復。
- **稽核鏈**（E1）：升級不需要任何動作——既有的 head 照常接受，升級後第一次寫入稽核紀錄時自動換成新的格式（在那之前錨點仍沒有保護）。升級後如果驗證出現 `head_mismatch` 或 `anchor_mismatch`，表示升級前稽核資料就已經與指標不一致（以前驗證不出來），請依 `KRS-E6201` 的處理方式以外送的鏈尾雜湊比對。**降版**：0.11.0（含）以前的程式看不懂新的 head、會略過它（不誤報，但沒有刪尾的偵測）；如果舊版在降版期間做了保留期限清除、之後沒有再寫入任何稽核紀錄就升回新版，新版會回報 `anchor_mismatch`。要降版請從備份還原（與其他版本的降版方式相同）。

## [0.11.0] - 2026-11-14

**Kairos 0.11.0、Pulse 0.11.0**：升級時段支援 IANA 時區、外部 URL 版本的資格預覽（`url_not_allowed`、`url_not_supported`）、Windows 服務與 MSI 的既有缺陷修正（ADR-0001 階段 0；**仍未在 Windows 實機驗證**）與安全稽核 F4。協定版本、8443 的 feature 與 8444 的 API 都不變；**Kairos 的資料庫升級到 schema v12，升級後 0.10.1 以前的 Kairos 無法開啟**（見下方「Kairos：遠端管理」）。

### Kairos：遠端管理

- **升級時段支援 IANA 時區名稱**（REMOTE.md 第 4.7.5 節「時段的時區與日光節約時間」、API.md 第 17.3 節）：推送的 `schedule.timezone` 除了 0.10.0 的固定位移（`+08:00`、`UTC`、`Z`，**照舊接受**，已建立的推送不受影響）之外，可以填時區名稱（`Asia/Taipei`、`Europe/Berlin`、`America/New_York`、`Australia/Sydney`…；大小寫須相同），時段依當地的時鐘計算，日光節約時間開始與結束時不必再手動調整。不認得的名稱在預覽、建立、修改時 400 `invalid_parameter`（`details.parameter = "schedule.timezone"`）。告警的 `alerts.timezone` 不變（仍是固定位移，也仍是推送沒有指定時區時的預設）。
  - **日光節約時間的規則**（決定性，每一條都有假時鐘的測試）：時段屬於它開始的那個當地日期，兩端各自換算成絕對時間（長度是牆上時鐘的長度，不是實際經過的時間）；落在**不存在的時刻**（撥快）的開始或結束換算為切換的那一刻——開始只會延後、不會提早；落在**重複的時刻**（撥慢）的開始取第二次、結束取開始之後的第一次；整個時段落在被跳過的區間時從當地的切換時刻起維持原本的長度，**時段不會變成零長度而消失**。
  - 預覽（`POST /api/v1/rollouts/preview`）與推送詳情新增 `upcoming_windows[]`：接下來 3 次時段實際的開始與結束（當地時間含當時的位移）、實際長度、是否受日光節約時間影響（`dst`：`gap`／`fold`）；預覽另有 `timezone`（`kind`、目前的位移、時區資料庫的版本）。主控台建立推送的表單新增「升級時段的時區」（可輸入或從建議清單選擇），並列出接下來的時段讓操作者在送出前確認。
  - **時區資料庫編進執行檔**（新依賴 `chrono-tz` 0.10.4，內含 tzdb 2025b；另帶入 `phf`、`phf_shared` 0.12，授權 MIT／Apache-2.0，`deny.toml` 不變）：不讀主機的 `/usr/share/zoneinfo`，Linux、musl 與 Windows 的結果相同，建置時沒有程式碼產生，發佈包維持可重現。Kairos 的執行檔約增加 2.2 MB（實測 x86_64 Linux 的 release 建置，`strip` 後 29.45 MB → 31.64 MB，＋7%）。**時區資料隨 Kairos 的版本更新**；某地臨時修改日光節約時間的規則而 Kairos 尚未更新時，可以先把推送的時區改為固定位移。
  - 不需要資料庫遷移（`rollouts.schedule` 是 JSON 文字欄位，SQLite 與 PostgreSQL 兩個後端原樣保存）、不需要協定變更（給 Pulse 的 `apply_window` 本來就是絕對時間）。**降回 0.10.1 以前的 Kairos 時**，舊版不認得時區名稱：請先把使用時區名稱的推送改回固定位移或取消。
- **外部 URL 版本的資格預覽事先列出不允許的主機**（REMOTE.md 第 2.1、4.1.2、4.7.2 節；ADR-0002 決策點 5c）：Pulse 在 hello 宣告本機是否允許從外部 URL 下載（見下方「Pulse：遠端管理」），Kairos 建立外部 URL 版本的推送時，把宣告「不允許」（本機 `upgrade.url_allowlist` 是空的）的主機在預覽與送出時分類為 **`url_not_allowed`**——以前它們算可升級，要到推送中才回報 `declined`（`PLS-W8411`）。**低於 0.10.0 的 Pulse**（不支援從外部 URL 下載，以前同樣在推送中 `declined`）事先分類為 **`url_not_supported`**（先以存放在 Kairos 的版本升級到 0.10 以上）。0.10.0 以上而沒有宣告（0.10.0、0.10.1 的 Pulse）與宣告有白名單的主機**照舊**算可升級，其中不允許這個 URL 的仍在推送中回報 `declined`、不計入失敗比例；預覽新增 `external_url`（`base_url`、`undeclared`、`allowlist`：可升級主機中沒有宣告與宣告有白名單的台數），主控台據此註明並附可複製的 `pulse.toml` 片段。由 Kairos 代管的版本不受影響；回滾檔的規則不變。主機 API（`GET /api/v1/hosts`、`…/{agent_id}`、`…/{agent_id}/upgrade`）的 `install` 多 `external_url`（`none`／`allowlist`／`null`），S05 的「升級」卡片顯示。
  - **資料庫升級到 schema v12**（SQLite 與 PostgreSQL 兩個後端）：`hosts` 新增一個可為 NULL 的欄位 `install_external_url`，第一次以新版啟動時自動升級，既有資料不變。**升級後 0.10.1 以前的 Kairos 無法開啟這個資料庫**（降版請從升級前的備份還原）；叢集請依 INSTALL「schema 改變的版本」的步驟（停止所有節點 → 升級 → 啟動）。

### Pulse：遠端管理

- **hello 宣告是否允許外部 URL**（REMOTE.md 第 2.1 節；PROTOCOL.md「hello 的 `install`」）：`install` 多一個選用欄位 `external_url`——`none`（本機 `upgrade.url_allowlist` 是空的，或沒有任何有效的項目）或 `allowlist`（至少有一個有效的前綴）。**只宣告「有沒有白名單」，不送出白名單的內容或雜湊**：白名單是「這台主機可以對哪些位址發出請求」的清單，集中到 Kairos 會讓被入侵的 Kairos 直接得知可用的外連目標。修改 `pulse.toml` 的白名單後重新啟動 Pulse 才會更新宣告；Pulse 接受或拒絕外部 URL 提議的條件不變。

### 相容性

- 協定版本與 8443 的 feature 都不變：`install.external_url` 是 hello `install` 物件的選用欄位。新的 Pulse 連 0.9／0.10 的 Kairos——舊 Kairos 依「`install` 的其他欄位忽略」略過它，行為與現在相同。0.10.1 以前的 Pulse 連新的 Kairos——沒有這個欄位視為「未知」，維持現行行為。`kairos-proto` 新增測試向量（PROTOCOL.md「範例／遠端管理：外部 URL 的宣告」）。

### Pulse：Windows（ADR-0001 階段 0；仍未在 Windows 實機驗證，見各項的註記）

- **更正：Windows 服務的失敗復原不是「一天內最多 3 次」**（ADR-0001 第 1.2 節 X1；影響 0.8.0～0.10.1）：[0.8.0] 的條目、`pulse service --help`、README 與 INSTALL 都寫「失敗時 10 秒後重新啟動，一天最多 3 次」，但那些版本設定的是三個「10 秒後重新啟動」的動作，而服務控制管理員在失敗次數超過動作數之後會**重複最後一個動作**——實際效果是每 10 秒重新啟動、**沒有次數上限**（設定或憑證錯誤時也一樣）；86400 秒只是「連續一天沒有失敗才把計數歸零」。歷史條目不改，以本條為準。
- **失敗復原改為 10／60／300 秒**（X1；`pulse/src/winsvc.rs` 的 `recovery`）：第 1 次失敗後等 10 秒、第 2 次 60 秒、第 3 次起每 300 秒重新啟動，沒有次數上限（監控代理程式持續嘗試恢復，但頻率有上限）；重設期 1 天與「非 0 結束代碼也算失敗」不變，所以設定或憑證錯誤（結束代碼 3／4）會每 300 秒重試一次，與 Linux 的 unit（這些代碼不重啟）不同。這是 ADR-0001 第 8 節 U13 (a) 在沒有啟動器時的等效設定，**政策尚待使用者決定**；三個數值集中在一處常數。
- **服務啟動時比對完整的失敗復原設定並校正**（X2）：`ensure_failure_actions()` 原本只在「沒有任何失敗動作」時補上，舊版註冊的服務（三個 10 秒）與被部分修改過的設定不會被修正；現在比對動作陣列、重設期與「非 0 結束代碼也算失敗」旗標，不一致就改回來並記一則日誌（先前靜默）。從 0.8.0～0.10.1 升級的服務在新版第一次啟動時自動改為 10／60／300 秒。副作用：管理者或群組原則自訂的失敗動作會在服務下次啟動時被改回來。另修正：校正時開啟服務的控制代碼少了 `SERVICE_START` 權限（設定含「重新啟動」的失敗動作需要它），由 MSI 註冊的服務可能因此一直沒有被補上失敗動作（未實機確認）。
- **MSI 升級、修復後服務會再啟動**（X4；`pulse/packaging/windows/pulse.wxs`）：0.8.0～0.10.1 的 MSI 只有 `ServiceControl Stop="both"`，升級或修復會停止服務而不再啟動（以 SCCM 升級後服務停到下次開機或手動啟動）。現在另加一列 `Start="install" Wait="no"`：安裝、升級、修復結束時啟動服務，只送出啟動要求、不等結果，服務啟動後才失敗不影響安裝的結束代碼。**行為變更**：第一次安裝後服務就會啟動（尚未 `pulse enroll` 時連預設的 `127.0.0.1:8080` 並持續重試；`pulse enroll` 之後重新啟動）。已確認的只有 wixl 0.106 產生的表（`ServiceControl` 的 Event／Wait、`StartServices` 的順序）；Windows Installer 的實際行為待 `windows-system` job 執行後確認。

- **Windows 系統層測試 S0 與 CI 的 `windows-system` job**（ADR-0001 第 6.1 節；`tests/windows/`、`.github/workflows/ci.yml`）：在 GitHub 代管的 `windows-2022`、`windows-2025` runner 上實際安裝 `package` job 產出的 MSI 與 zip——服務的命令列與帳號、啟停、失敗復原的設定與連續結束服務行程 5 次（X1、X2）、MSI 修復與 MajorUpgrade 後的服務狀態（X4；另以版本 0.0.1 的舊式 MSI 重現原本的行為）、狀態目錄的 ACL 與不合規時的拒絕（F4）、移除後的殘留；`icacls`、`sc.exe qfailure`、MSI 日誌與結果表存成 artifact。沒有 Kairos，不驗證註冊、連線與回報；需要重開機的項目（X3）在 VM（`docs/windows-vm-validation.md`）。**腳本只通過語法剖析，這個 job 尚未在 runner 上執行過**。

### 安全修正

- **Windows：`%ProgramData%\Pulse` 在使用前檢查擁有者、DACL 與重新解析點**（安全稽核 F4，中；`pulse/src/state_dir.rs`；影響 0.8.0～0.10.1）：`%ProgramData%` 的預設 ACL 讓一般使用者可以建立子目錄，而 LocalSystem 的服務在第一次啟動時才建立 `%ProgramData%\Pulse`、之後也不檢查它。一般使用者搶先建立這個目錄後，服務會在其中寫 `status.json`（並以路徑修改 ACL）、讀取並套用集中設定的快取 `remote\config.json`——**不需要管理權限就能降低這台主機的監控**——`pulse status` 與 SCCM 的健康檢查也會讀到偽造的狀態檔；即使目錄由服務建立，繼承的 ACL 也讓 Users 能在裡面建立檔案與子目錄。修正：
  - 使用前檢查目錄本身：擁有者是 SYSTEM、Administrators 或 TrustedInstaller；DACL 不是 NULL，寫入類權限只給這三者（只用於繼承的項目也算；只看允許項目，可能多拒絕、不會漏放）；不是重新解析點（以 `FILE_FLAG_OPEN_REPARSE_POINT` 開啟，屬性、擁有者與 DACL 取自同一個控制代碼）。
  - 目錄不存在時，服務以明確的安全描述元建立（擁有者 Administrators；SYSTEM、Administrators 完全控制；Users 只能讀取目錄本身），不再繼承 `%ProgramData%` 的 ACL。唯讀的命令（`pulse status` 等）不建立目錄。
  - **不合規時拒絕使用**，代碼 `PLS-E8207`（工程暫定號）：服務照常執行，狀態檔改寫在工作目錄的 `logs\status.json`、設定快取改放 `logs\remote\`；`pulse status` 不讀該目錄中的狀態檔。
  - **升級注意**：0.8.0～0.10.1 建立的 `%ProgramData%\Pulse` 繼承了 Users 的寫入權限，升級後**會被拒絕**（`PLS-E8207`）；以系統管理員停止服務、`rd /s /q "%ProgramData%\Pulse"`、啟動服務即可（`pulse/packaging/INSTALL.md` A.3）。
  - 沒有涵蓋：`%ProgramData%` 本身的 ACL、目錄內既有檔案的擁有者（處理方式是刪除整個目錄）、`status.file` 明確指定的路徑。信任錨的預設位置也在這個目錄（Windows 的遠端升級尚未開放，走不到；ADR-0001 第 4.5 節已決定改放安裝目錄）。
  - 驗證：判斷邏輯是純函式，在 Linux 上以單元測試涵蓋（一般使用者擁有、Users 可寫、只用於繼承的寫入項目、NULL DACL、重新解析點、損毀的 ACL 都被拒絕）；讀取安全描述元與建立目錄的 Windows 程式只經過交叉編譯與 clippy，**尚未在 Windows 上執行**（Windows 上的單元測試與 `tests/windows/` 的情境待 CI）。
  - MSI 沒有改為在安裝時建立這個目錄：wixl 0.106 不支援 `<Permission>`／`<PermissionEx>`。以 `msibuild` 匯入 `MsiLockPermissionsEx` 表在技術上可行（已確認匯入成功且輸出可重現），但沒有 Windows 可以驗證安裝行為，這一輪不採用；MSI 安裝後服務會立即啟動並自行建立目錄，「安裝後、第一次啟動前」的空窗已大幅縮短。

## [0.10.1] - 2026-11-14

**Kairos 0.10.1、Pulse 0.10.1**：0.10.0 之後的四項補強（H1～H4）。協定版本、8443 的 feature 與 8444 的 API 都不變。

### Kairos：告警

- **「雙人覆核已關閉」不可靜音、不可停用**（H1；`kairos/ALERTS.md` 第 5.3 節「受保護的系統規則」）：0.10.0 的系統規則 `sys.rollout_approval_disabled` 可以被預先靜音或停用，關閉雙人覆核就不會推播。0.10.1 起它的事件不受任何靜音影響（包含升級前建立、明確選取它的靜音），停用它的 API 回 409 `rule_always_enabled`（並寫一筆結果為 `rejected` 的稽核）；建立或修改靜音時選取不可靜音的規則回 400 `rule_unsilenceable`（`sys.unauthenticated` 也是——它原本就不受靜音影響，只是以前可以建立一筆沒有作用的靜音）。保護由程式碼決定、不依賴資料庫裡那一列的內容：升級上來的環境在啟動時把這一列同步回來（已被停用的重新啟用、補上 `params.unsilenceable`），引擎每次載入規則再套一次；**不需要資料庫遷移**。規則的 API 多 `unsilenceable`、`always_enabled` 兩個欄位；主控台的規則清單對這條規則顯示「啟用（不可停用）」而不是開關，靜音表單不能勾選不可靜音的規則，這類事件的詳情不提供「建立靜音」。通知管道與通知設定仍可調整（沒有接管道就不會推播，事件照常記錄）。
- **證據類的安全告警不可靜音**（安全稽核 B5；`kairos/ALERTS.md` 第 5.3 節）：`sys.audit_integrity`（稽核雜湊鏈斷裂或被竄改）與 `sys.release_signature_failed`（發佈簽章驗證失敗）0.10.1 起比照 `sys.unauthenticated` 設為不可靜音，避免被盜用的管理員工作階段在動手前先建立一筆靜音讓動作不留下告警。機制與上一項相同（由程式碼決定、啟動時同步既有列、不需資料庫遷移）；兩者仍可停用。偏操作性、可能誤報的安全相關告警（`sys.cert_rejected`、各 `sys.enroll_*`、`sys.tls_reload_failed` 等）維持可靜音。

### Kairos：遠端管理

- **雙人覆核關閉時開始的推送，在稽核與主控台看得出來**（H2；`crates/kairos-proto/REMOTE.md` 第 5.3 節）：0.10.0 對「關閉雙人覆核」的偵測只靠 `meta` 表記錄的前值，能寫資料庫的人把前值改掉就沒有痕跡。0.10.1 起稽核 `rollout.submit` 的 `details` 記下送出當下的 `rollout_approval`，`rollout.start`（實際開始的那一刻，不論由送出者、核准者或排程的 `system` 寫入）記下開始當下的 `rollout_approval`、`approval`、`submitted_by`、`approved_by`——在 `none` 之下開始的每一個推送各自在雜湊鏈（HMAC）裡留下紀錄，另記一行警告日誌 `rollout.start_unapproved`。推送的 API（清單、詳情、SSE）多 `approval`：`approved`、`not_required`（送出時為 `none`，沒有核准就排程或開始）、`unknown`（0.9.x 的推送沒有送出紀錄，不誤標）、`null`；詳情另有目前的 `rollout_approval`。主控台的推送清單與詳情對 `not_required` 標示「未經核准」：目前的設定是 `required` 時以警告呈現，否則是中性的標籤與說明。**沒有 schema 變更**（仍是 v11）：`approval` 由既有的 `submitted_at`／`approved_at`／`started_at` 導出，另存一欄不會多出資訊。
- **推送清單與計數泡泡的資料庫負擔**（H3；API.md 第 17.3 節；0.9.0 就有的問題）：`GET /api/v1/rollouts` 原本對清單中的**每一筆**推送載入全部主機列（JOIN 清冊）只為了算各狀態的台數，而主控台側邊導覽的計數泡泡每 30 秒、加上每次 SSE `rollout.progress` 都呼叫不帶篩選的這個端點。0.10.1 起：(1) 清單的 `progress`、`hosts_total`、`rollback_failed_unacknowledged` 與 `next_action` 的 `waiting_artifact` 由資料庫以一個 `GROUP BY` 查詢彙總（SQLite 與 PostgreSQL 都是；回應的格式與數字不變），只有待核准的推送仍讀主機列；(2) 新的 `GET /api/v1/rollouts/summary`（`rollouts:read`）只回計數——清單 `meta` 的全部欄位與各狀態的推送數 `states`——只做 `COUNT` 查詢；計數泡泡改查它。清單仍然沒有分頁（推送保留 400 天）；多狀態篩選、檢視與分頁是之後的工作（ADR-0002 S4）。沒有量測改動前後的差異（沒有現成的工具量這個端點）。

### Pulse

- **升級輔助程式：接續時健康確認的期限已過，先確認健康再決定回滾**（H4；**設計變更**，`crates/kairos-proto/REMOTE.md` 第 17.9.3 節「接續 `replaced` 時的健康確認」、第 17.9.4 節第 13、14 列、第 17.9.10 節；`docs/adr/0003-rollback-bundle-trust.md` 第 14 節）：0.10.0 的輔助程式接續 `replaced` 的交易時，期限已過就不做任何檢查、直接回滾——輔助程式被終止很久才再被觸發、主機關機很久才開機，或紀錄不可用／路徑不安全由人修好之後才接續時，會退掉一個可能已經健康運作的升級。0.10.1 起接續時至少有 120 秒（`STABLE_FOR` 60 秒＋60 秒；等於推送可以設定的最短健康確認期限）做**與正常流程相同**的健康確認：通過 → `succeeded`，不通過 → 回滾。健康條件沒有放寬；沒有中斷的正常流程、`rolling_back` 的接續、回滾的來源與目標都不變。
- **接續時重新判斷崩潰偵測的基準**（同上）：`NRestarts` 在重新開機後從 0 起算，0.10.0 沿用替換前量的基準，開機前若曾自動重啟過 k 次，新版在開機後可以崩潰 k 次而不被這一項發現。0.10.1 在交易紀錄的 `replaced` 多記 `boot_id`，接續時**先判斷接續之前有沒有崩潰重啟過**（同一次開機中計數增加、重新開機後有任何自動重啟、或計數被重設後又不是 0 → `crash_restart`，回滾），沒有證據時才決定之後的基準（重新開機或計數被重設後為 0）。殘餘的窗口：由 0.10.0 的輔助程式替換、0.10.1 接續的那一次升級（紀錄沒有 `boot_id`），重新開機後恰好崩潰了與替換前相同的次數之後穩定下來時，這一項看不出來（與 0.10.0 相同）。
- 系統層測試（`tests/systemd/`）新增情境 `late-resume`（j）與測試用的版本 test.8：輔助程式記下 `replaced` 後終止主機，關機到期限過後才開機 → 三台 `succeeded`。

### 安全修正（夜間稽核 2026-11）

這些是 2026-11 夜間安全稽核（見原始碼 repo 的 `handoff/SECURITY-AUDIT-2026-11-14.md`）中、0.10.0 的兩個「高」（B1、A1）之外排進 0.10.1 的修正。另見上方「雙人覆核已關閉」（H1）與「證據類的安全告警不可靜音」（B5）。

- **單一主機無法再耗盡全域序列表**（安全稽核 N8；Kairos）：`report` 送進新序列時，原本先把序列寫進全域 `series` 表（上限 1,000,000）才檢查每主機上限（預設 2048），單一已註冊主機可在約 33 分鐘內塞滿全域表，之後任何主機回報含沒見過的序列（新掛載點、新介面、新主機）都被拒（`storage_unavailable`）且無法復原。0.10.1 起新序列只在這台主機未達每主機上限時才寫入全域表，單一主機最多讓全域表增加 `storage.max_series_per_host` 筆。SQLite 與 PostgreSQL 都修；行為對未超限的主機不變。
- **`kairos db restore-keys`、`kairos cluster keys import` 以 root 執行時拒絕**（安全稽核 F2；Kairos）：這兩個寫資料庫的 CLI 原本沒有其他 CLI 都有的 root 防呆。它們會以 root 寫回金鑰檔、建立 `data/pki` 並沿路徑把擁有者改成服務帳號；服務帳號可先把目標換成符號連結，誘使 root 對任意路徑（例如 `/etc`）`chown`。0.10.1 起以 root 執行而資料目錄屬於服務帳號時拒絕（`sudo -u kairos`）；資料目錄屬於 root 時 chown 本來就是 no-op。
- **文字格式的記錄跳脫訊息中的控制字元**（安全稽核 E6；共用的 `kp-obs`）：SMTP／OTLP 伺服器的回應會被回顯進記錄的訊息，其中的 CR／LF 可在文字格式的日誌檔中偽造出額外的日誌行。0.10.1 起訊息中的控制字元一律跳脫（欄位值本來就以 JSON 編碼）。

## [0.10.0] - 2026-11-14

**Kairos 0.10.0、Pulse 0.10.0**：衝刺 10——**PostgreSQL 儲存後端**（高可用 L1b，`kairos/STORAGE.md` 第 19 節）、**叢集模式**（高可用 L2，階段 A：主動／備援，`kairos/HA.md` 第 6.8、17.1 節；**HA 預覽**）與**遠端管理 F8 的其餘項目**（`crates/kairos-proto/REMOTE.md`：`kairos releases` 命令列、推送的排程與升級時段、自訂批次、雙人覆核、外部 URL 的版本庫、升級中主機的離線告警抑制、SSE `rollout.host`、版本庫檔案的健康檢查）。**協定版本不變（v1～v2）、沒有新的協定 feature**，8444 的 API 不變，0.9 與 0.10 的 Kairos／Pulse 任意組合都能連線；Kairos 的資料庫升級到 **schema v11**（v9 → v10 → v11，不可逆，升級前先備份）。預設值（`storage.backend = "sqlite"`、`cluster.enabled = false`、`remote.rollout_approval = "none"`）下不需要修改設定檔。

**叢集模式是 HA 預覽**：只在一台測試機上以兩個 Kairos、一個沒有備庫的 PostgreSQL 驗證過；資料庫主備切換、真正的網路分割、L4 負載平衡器與虛擬 IP 等情境還沒有實測（下方「已知限制」與 HA.md 第 17.1 節）。上線前必須在實際的拓撲上演練。

### 破壞性變更與升級步驟（升級前必讀）

| # | 產品 | 變更 | 誰會受影響 | 升級步驟 |
|---|---|---|---|---|
| 1 | Kairos | **資料庫升級到 schema v11，不可逆**：第一次以 0.10.0 啟動（或任何開啟資料庫的 CLI）時，在一個交易中依序套用 v10（叢集協調的資料表與三個可為 NULL 的欄位；單機與 SQLite 上保持空白）與 v11（`rollouts` 新增 `submitted_at`、`submitted_by`、`reject_reason`）。升級後 0.9 的 Kairos 無法開啟 | 所有部署 | 升級前 `kairos db backup <檔案> --include-keys --password-stdin < <密碼檔>`；降版時從這份備份還原 |
| 2 | Kairos | **升級推送中的主機不再因為替換與重新啟動而開離線事件**（行為變更）：主機從交給輔助程式（`applying`）到回報結果之間，系統規則與自訂的 `host_offline`／`host_long_offline` 規則不開新事件，也不計入網段離線、大規模離線的台數。抑制最長到 `applying` 之後 `health_timeout_secs + 10 分鐘`，超過後仍離線就照常告警 | 以「主機離線」通知確認遠端升級正在進行，或在升級期間依賴離線告警的部署 | 不需要修改設定（沒有開關）。升級的進度請看推送詳情或 SSE `rollout.progress`／`rollout.host`；其他規則（指標門檻、回報延遲…）不受影響 |
| 3 | Kairos | **API 的列舉值增加**：推送的 `state` 多了 `scheduled`、`pending_approval`，`next_action.code` 多了 `scheduled`、`waiting_window`，告警處理紀錄的動作多了 `upgrade_suppressed`；推送的本文帶 `schedule` 不再回 400，`waves` 由最多 2 批放寬為最多 10 批 | 列舉這些值、遇到不認得的值就報錯的整合（Webhook 接收端、自行呼叫 `/api/v1/rollouts` 的工具）；依「帶 `schedule` 會被拒絕」寫的腳本 | 讀取端忽略不認得的 `state`、`next_action.code` 與處理紀錄的動作；`windows`、`starts_at`、`ends_at`、`timezone` 放在 `schedule` 物件內 |

Pulse 沒有破壞性變更（發佈清單的 `breaking` 為 `false`）。**從 0.10.0 開始的下一次遠端升級**（0.10.0 → 之後的版本；0.9 → 0.10 由 0.9 的輔助程式執行，不受影響），升級輔助程式會先檢查路徑（「Pulse」一節）：`/opt/pulse`、`/opt/pulse/pulse`、`/var/lib/pulse-upgrade` 與它們的每一層上層目錄必須屬於 root、群組與其他人不可寫（上層目錄與 `/opt/pulse` 設了 sticky 位元時例外，例如 `1777`；`/opt/pulse/pulse` 與 `/var/lib/pulse-upgrade` 沒有這個例外），`/opt/pulse` 與 `/var/lib/pulse-upgrade` 不得是符號連結，`/opt/pulse/pulse` 必須是一般檔案。依 INSTALL 或套件安裝的主機符合這些條件。不符合時遠端升級以 `PLS-E8405`（`unsafe_path: <路徑>: <原因>`）失敗，不做任何變更，推送會依失敗比例自動暫停；手動升級不受影響。升級到 0.10.0 之後**不必等到推送失敗才發現**：0.10.0 的 Pulse 在每次連線時事先做同一套檢查，不符合的主機在 hello 回報 `install.upgrade = "unsafe_path"`，主控台的資格預覽（建立推送時）與主機詳情會標出「路徑不安全」與處置方式（「Pulse」一節）；在主機上可以用 `pulse upgrade status`，或 `stat -c '%U %a %n' / /opt /opt/pulse /opt/pulse/pulse /var /var/lib /var/lib/pulse-upgrade` 自行檢查。

**升級步驟（單機，SQLite；v9 → v11）**：

1. 備份：`kairos db backup <檔案> --include-keys --password-stdin < <密碼檔>`（版本庫的發佈檔 `data/releases/` 不在備份中，需要時另外複製）。
2. 換上 Kairos 0.10.0（tar.gz 照 INSTALL 第 11 節換執行檔；deb／rpm 直接升級）並啟動：schema 由 v9 依序升級到 v10、v11（只新增空的資料表與可為 NULL 的欄位，一個交易）。升級完成後，0.9 的 Kairos 就無法再開啟這個資料庫（SQLite：`PRAGMA user_version` 為 11）。
3. 換上 Pulse 0.10.0：0.9.0 的主機可以由 Kairos 推送（`min_upgrade_from` 仍是 0.9.0），或照舊手動升級。0.9 的 Pulse 可以繼續使用——排程、升級時段、自訂批次與雙人覆核都不需要升級 Pulse；只有外部 URL 的版本庫需要 Pulse 0.10.0。
4. 需要時再啟用新功能：雙人覆核（`[remote] rollout_approval = "required"`，需要重新啟動；至少要有兩位管理員帳號，而且建立者與送出者都不能核准——只有兩位管理員時由同一個人建立並送出。啟用時 Kairos 會寫入稽核 `settings.rollout_approval.change`；之後改回 `none` 會開告警事件「雙人覆核已關閉」，部署建議見 INSTALL「啟用雙人覆核時的部署建議」）、外部 URL 的版本庫（每台 Pulse 的 `[upgrade] url_allowlist`）。

**改用 PostgreSQL**（選用；`kairos/packaging/INSTALL.md` 附錄 D）：先完成上面的步驟 1、2（`kairos db migrate` 只搬移 v11 的資料庫），停止 Kairos → `kairos db migrate --to postgres` → 設定 `storage.backend = "postgres"` 與 `[storage.postgres]` → 啟動。版本庫的發佈檔與金鑰檔仍在資料目錄，不在資料庫中。

**叢集部署**（選用，HA 預覽；INSTALL 附錄 H）：需要 PostgreSQL 後端。`kairos cluster init` → 以 `kairos cluster keys export`／`import` 把金鑰分發到其他節點 → 各節點設定 `[cluster] enabled = true` 後啟動。**每個節點都要各自匯入相同的 Pulse 發佈版本**（`kairos releases import`；見「已知限制」）。之後 schema 改變的版本要先停止所有節點、升級所有節點的執行檔，再啟動第一個節點（由它升級 schema）。

### 相容性

- 協定（v1～v2）、8443 的 feature、8444 的 API 都與 0.9.0 相同；0.9 與 0.10 的 Kairos／Pulse 任意組合都能連線（下表）。
- **schema v11**（SQLite 與 PostgreSQL 都是）。PostgreSQL 後端從 v9 開始，有 v9 → v10 → v11 的升級路徑（由第一個以新版啟動的節點或開啟資料庫的 CLI 執行；0.10.0 開發期間建立的 v9、v10 schema 也會升級）。
- 預設值下的行為與 0.9 相同，差異只有破壞性變更第 2、3 項，以及：
  - `/readyz` 的 `checks` 多了 `cluster`、`keys`、`schema` 三個欄位（單機為 `disabled`、`ok`、`ok`）；`/metrics` 多了 `kairos_cluster_enabled`（單機為 0）與 `kairos_db_*`。
  - 系統健康 `GET /api/v1/system/health` 的 `database` 項目多了 `values.backend`、`values.postgres`，`remote` 項目多了 `values.release_files_missing`、`missing_files[]`（新增欄位，既有欄位不變）；叢集模式另有 `cluster` 項目。
  - `GET /api/v1/hosts` 多了 `meta.recommended_min_agent_version` 與每一列的 `agent_outdated`；`GET /api/v1/releases` 的每個版本多了 `base_url`、`artifacts[]` 多了 `registered`；`POST /api/v1/releases` 的 `source: "external"` 不再回 400（需要 `base_url`）；`GET /api/v1/rollouts`、`GET /api/v1/config/profiles` 多了查詢參數 `q`。
  - 重複上傳同一個發佈檔（`PUT /api/v1/releases/{version}/artifacts/{filename}`、`kairos releases import`）時，版本庫中已有同名而大小不同的檔案會被取代（0.9.0 一律保留既有的檔案，被截斷的檔案無法以重新上傳修復）。
  - `kairos db restore-keys` 另外接受 `kairos.keys-backup/v2`（`kairos cluster keys export` 的輸出）。
  - `storage.jsonl_mirror` 的錯誤訊息文字改為「只在 backend = "sqlite" 或 "postgres" 時有效」。
- 新權限 **`rollouts:approve`**（只有內建的管理員；`Permission::ALL` 為 46 個）；0.4／0.5 的舊範圍 token 不會取得。API token 不能核准推送（403 `session_required`）。
- **外部 URL 的版本庫**沒有新的協定 feature：0.9.0 的 Pulse（或沒有設定 `url_allowlist` 的 0.10）收到外部 URL 的提議時回報 `declined`，不計入失敗比例；Kairos 目前無法在預覽時得知哪些主機允許外部 URL。`[upgrade] url_allowlist` 在 Pulse 0.9.0 已可設定但沒有作用，0.10.0 起生效（預設空＝只從 Kairos 下載）。
- PostgreSQL 後端沒有啟用叢集模式時，**同一個資料庫與 schema 同時只能有一個 Kairos 使用**——第二個 Kairos 拒絕啟動（`KRS-E4402`，結束代碼 3）；叢集的節點同樣不能加入正被單機模式使用的 schema。

| Pulse ＼ Kairos | Kairos 0.9 | Kairos 0.10 |
|---|---|---|
| **Pulse 0.9** | 不變 | 與 0.9 相同，另外可以使用推送的排程、升級時段（Pulse 0.9.0 就遵守 `apply_window`）、自訂批次與雙人覆核；外部 URL 的提議回報 `declined` |
| **Pulse 0.10** | 與 0.9 相同（Kairos 0.9 不會提議外部 URL；安裝路徑不安全的主機回報 `install.upgrade = "unsafe_path"`，Kairos 0.9 分類為 `not_ready`、顯示「其他」） | 全部功能；外部 URL 的版本庫另需要 Pulse 本機的 `upgrade.url_allowlist`。叢集模式下協商了 `cluster` 的 Pulse 在 `hello_ack` 收到 `cluster_id` |

### Kairos：PostgreSQL 儲存後端（HA L1b；`kairos/HA.md` 第 4、5、7、12 節、`kairos/STORAGE.md` 第 19 節）

- **`storage.backend = "postgres"`**：SQLite 後端保存的所有資料（原始 report、彙總、主機清冊、資產、告警、使用者與工作階段、稽核雜湊鏈、註冊與 PKI、設定、指標轉出設定、遠端管理）可以改放在外部的 **PostgreSQL 14 以上**（只用社群版的功能，不需要擴充）。API、CLI、協定與 SQLite 後端相同。**SQLite 仍是預設值，既有部署的行為不變**。
- **`[storage.postgres]`**：`host`（或 Unix socket 的目錄）、`port`、`dbname`、`user`、`password_file`、`schema`、`sslmode`（`verify-full`／`require`／`disable`；未設定時本機為 `disable`、其他為 `verify-full`）、`ca_file`、`pool_size`（預設 16）、`connect_timeout_secs`。密碼只從檔案讀取（每次建立連線時重新讀），不寫入日誌、`kairos diag` 與 `--check-config` 的輸出。TLS 使用 rustls＋ring（與其他連接埠相同）；連線帶 TCP keepalive（10 秒 × 3 次）與 `tcp_user_timeout`（30 秒）。
- **schema** 與 SQLite 一對一（資料表、欄位、索引名稱相同；文字欄位為 `COLLATE "C"`，排序與分頁的結果與 SQLite 一致），第一次啟動時在空的 schema 上建立。schema 比程式新（`KRS-E4405`）、不是 Kairos 的 schema（`KRS-E4402`）或 `kairos db migrate` 尚未完成（`KRS-E4404`）時拒絕使用。
- **並行控制**：序列化的寫入交易以交易層級的 advisory lock 排成一列（等同 SQLite 的 `BEGIN IMMEDIATE`）；稽核雜湊鏈另有自己的鎖，`id` 在持鎖期間指定（連續、沒有空號），雜湊與 HMAC 與 SQLite 完全相同。report 的寫入不經過這些鎖：一批一個交易，以多列 `INSERT … ON CONFLICT DO NOTHING RETURNING` 去重；死結與序列化失敗自動重試，commit 時連線中斷為結果不明（Pulse 重送後去重）。
- **`kairos db migrate`**（新）：`--to postgres`（SQLite → PostgreSQL）與 `--to sqlite:<檔案>`（PostgreSQL → SQLite），停止 Kairos 後執行。保留所有主鍵值；完成後一定驗證每張表的列數、`reports` 的 id 範圍與抽樣 1,000 列、稽核鏈（與來源的驗證結果相同），不一致時目的端不可使用（`KRS-E4404`）。`--reports-since <期間>` 只搬最近的原始資料；`--dry-run`、`--restart`、`--json`（`kairos.db.migrate/v1`）。Kairos 執行中時 `KRS-E4304`。來源與目的都必須是 v11。
- **啟動與檢查**：啟動時檢查版本、編碼、是否連到主庫（`KRS-E4402`）；連不上為 `KRS-E4401`（結束代碼 3）。沒有同步複寫時照常啟動，啟動時與之後每小時記一次 `KRS-W4403`；時差超過 2 秒記 `KRS-W8305`；`max_connections` 不足、連線未加密或不驗證憑證記 `WARN`。`kairos --check-config` 新增 `database` 項目（實際連線，不修改任何東西）。
- **可觀測性**：`/readyz` 的 `database` 檢查連線池、主庫與 schema；`/metrics` 新增 `kairos_db_pool_connections{state}`、`kairos_db_pool_size`、`kairos_db_pool_errors_total{kind}`、`kairos_db_transaction_retries_total{reason}`、`kairos_db_clock_skew_seconds`、`kairos_db_synchronous_replication`；系統健康的 `database` 項目新增 `values.backend` 與 `values.postgres`（SQLite 時 `backend` 為 `sqlite`、`postgres` 為 `null`）。
- **容量上限**（`storage.max_total_bytes`）在 PostgreSQL 上以資料表與索引的大小計算，刪除改為估計制（PostgreSQL 刪除後檔案不會變小；STORAGE.md 第 19.5 節）。
- **不支援的命令**：`kairos db backup`（改用 `pg_dump` 等工具，或 `kairos db migrate --to sqlite:<檔案>`）與 `kairos reports import`（先匯入 SQLite 再搬移）在 PostgreSQL 後端以 `KRS-E4406`〔暫定〕拒絕。
- **遠端管理在 PostgreSQL 上**：0.10.0 的遠端管理（下方）在兩個後端的行為相同——`RemoteStore` 的每個操作（含排程、雙人覆核、外部 URL 的版本、替換中的主機）由同一個測試本體在 SQLite 與 PostgreSQL 各跑一次並比對結果。
- 與 HA.md 規格不同之處列在 HA.md 第 5.7 節；安裝與搬移步驟見 INSTALL 附錄 D。

### Kairos：叢集模式（高可用，階段 A：主動／備援；HA L2；`kairos/HA.md` 第 3.2、6、11、13～17 節）〔HA 預覽〕

- **`[cluster] enabled = true`**（需要 `storage.backend = "postgres"`）：兩個以上的 Kairos 共用一個 PostgreSQL，以資料庫中的租約（`cluster_leases`）選出唯一的**主動節點（leader）**。只有它監聽 agent（8443）、註冊（8444）與主控台（9443）並執行所有背景工作；其餘是**備援節點**：連著資料庫、寫心跳、每 2 秒嘗試取得租約，只監聽 `observability.listen`（`/healthz` 200、`/readyz` 503）。不需要 etcd 等額外元件；租約的時間一律用資料庫的時鐘。
- **容錯移轉**：主動節點失效後，租約在 `cluster.lease_ttl_secs`（預設 15 秒）到期，備援在 2 秒內取得租約，依單機的啟動流程載入狀態並開始監聽；Pulse 經虛擬 IP、L4 負載平衡器或 `server.fallback_addresses` 重連，停機期間的取樣由本機緩衝補送（不遺失、不重複）。在 kp-build 上的端對端測試（兩個 Kairos＋Pulse＋PostgreSQL，SIGKILL 主動節點）連跑 8 次（PostgreSQL 14 與 17）：備援在 12.4～12.5 秒取得租約、12.8～12.9 秒開始監聽，Pulse 在 13～15 秒重新註冊；`step-down` 後約 2.3 秒換手；凍結超過租約時間的舊主動節點恢復後 0.02 秒內結束。正常關機與 `kairos cluster step-down` 會交還租約，接手不必等租約到期。
- **不會有兩個節點同時寫入（fencing）**：主動節點的每個可寫交易與自動提交的寫入敘述都先以 `FOR SHARE` 確認租約仍是自己的；舊的主動節點（暫停後恢復、與資料庫的網路中斷後恢復）的寫入會失敗，而接手的節點要等舊節點進行中的交易結束才取得租約。**失去租約的節點立即以結束代碼 75 結束**（`KRS-W8502`：續約回報租約已不屬於自己、超過 `lease_ttl_secs − 5` 秒沒有續約成功、或任何寫入的 fencing 檢查失敗），由 systemd 的 `Restart=on-failure` 重新啟動為備援。
- **金鑰**：叢集模式下**不再自動建立金鑰檔**（`alerts-secrets.key`、`console.key`、`audit-hmac.key`、`privacy.key`）。`kairos cluster init` 在第一個節點建立金鑰與 `cluster_id`，並把金鑰指紋寫入資料庫；其他節點以 `kairos cluster keys export`／`import`（密碼加密，格式 `kairos.keys-backup/v2`，含簽發 CA 私鑰）取得同一組金鑰。金鑰缺少或不符的節點記 `KRS-E8504`、`/readyz` 回 503（`keys: mismatch`）、不參與 leader 選舉；修正後不需要重新啟動。
- **`kairos cluster`**（新）：`status [--json]`（節點、leader、金鑰與設定的一致性；`kairos.cluster.status/v1`）、`init`、`keys export|import`、`drain <node_id>`、`step-down`、`forget <node_id>`。
- **啟動保護**：沒有啟用叢集模式的 Kairos 連到已有另一個 Kairos（單機或叢集的節點）在使用的 schema 時拒絕啟動（`KRS-E4402`，結束代碼 3）；叢集的節點同樣不能加入正被單機模式使用的 schema。
- **可觀測性**：`/readyz` 的 `checks` 新增 `cluster`（`leader`／`standby`／`draining`／`lost`；單機為 `disabled`）、`keys`、`schema`，並支援 `?scope=agent|enroll|console|leader`（keepalived 的 `track_script` 用 `scope=leader`）；`/metrics` 新增 `kairos_cluster_enabled`、`kairos_cluster_role{role}`、`kairos_cluster_lease_epoch`、`kairos_cluster_lease_renewals_total{result}`、`kairos_cluster_leader_changes_total`、`kairos_cluster_nodes{state}`、`kairos_cluster_keys_ok`；系統健康新增 `cluster` 項目（沒有 leader、金鑰不一致為嚴重；節點失聯、設定不一致、沒有同步複寫、時差、只有一個節點為警告），另有 `GET /api/v1/cluster`（`settings:read`）；JSON 格式的日誌每行多一個 `node_id`。
- **其他**：節點的設定雜湊不同時記 `KRS-W8505`（只警告）；心跳逾時的節點由 leader 標為失聯（`KRS-W8506`）；`node_id` 與另一台主機上執行中的節點重複時拒絕啟動（`KRS-E8503`）；協商了 `cluster` 的 Pulse 在 `hello_ack` 收到 `cluster_id`；主控台的 SSE 在接手後以 `resync("failover")` 重新載入；告警在換手時記「叢集的 leader 已由 A 切換到 B」（`KRS-I8501`），不開「停機」事件；`--check-config` 檢查叢集是否已建立、金鑰是否一致、`cluster.service_name` 是否在伺服器憑證的 SAN 中。
- **schema v10**（SQLite 與 PostgreSQL）：新增叢集的資料表（`cluster_nodes`、`cluster_leases`、`cluster_revs`、`host_presence`、`cluster_events`、`alert_event_seq`、`login_challenges`、`enroll_nonces`、`auth_failures`）與三個欄位（`enroll_requests.issuing_node`、`issuing_at`、`alert_deliveries.claim_epoch`）；這一版只使用 `cluster_nodes` 與 `cluster_leases`，其餘先建立、保持空白。
- **遠端管理在叢集模式下**（HA.md 第 6.8 節的盤點）：推送引擎、排程與升級時段、離線告警抑制的調和、SSE 的差異比對都只在主動節點的行程中執行，狀態在資料庫；換手後由接手的節點從資料庫繼續——已排程的推送到 `starts_at` 開始、替換中（`applying`）主機的離線抑制在接手後約 5 秒內重建。端對端測試 `remote_upgrade_state_survives_failover` 以兩次換手驗證。`[remote] rollout_approval` 納入「需要一致的設定」（不一致時 `KRS-W8505`）。版本庫的發佈檔是節點本機的檔案（見「已知限制」）。
- 部署步驟：INSTALL 附錄 H。

### Kairos：遠端管理 F8 的其餘項目（`crates/kairos-proto/REMOTE.md`、API.md 第 17 節）

- **`kairos releases list|import|delete`**（REMOTE.md 第 8.2、13.1 節）：Pulse 發佈版本庫的命令列。`import <目錄>` 讀取同一次發佈的 `SHA256SUMS`、`SHA256SUMS.sig`、`pulse-*-release.json` 與發佈檔，可以重複執行補上發佈檔，也可以在 Kairos 啟動前先匯入（離線環境）；`list [--json]` 列出版本、發佈檔與使用情形，並標出已存入而檔案遺失或大小不符的發佈檔；`delete <版本> --confirm <版本>`（L2）。驗證、`release_in_use` 的保護與稽核事件與主控台 API 相同，執行者為 `cli:<OS 帳號>`。`import --external <URL>` 登記外部 URL 的版本庫。兩個儲存後端都可以使用；叢集模式下可以在任何節點（含備援）執行。
- **升級推送的排程與升級時段**（REMOTE.md 第 4.7.1、4.7.3、4.7.5、13.1 節、API.md 第 17.3 節）：推送新增 `schedule`——`starts_at`（在這之前為新的狀態 `scheduled`，到時由推送引擎開始）、`ends_at`（到時推送結束，尚未開始替換的主機標為 `expired`／`rollout_ended`）、`windows[]`（每週的升級時段：星期、`HH:MM` 的開始與結束，可跨午夜）與 `timezone`（**固定位移**，預設 `alerts.timezone`）。時段是「允許開始替換的時間」：時段外不提議新的主機，提議的 `apply_window` 到時段結束為止，已開始替換的照常完成；時段結束時還沒開始的主機在下一個時段重新提議，不計入失敗。`next_action` 新增 `scheduled`、`waiting_window`；已排程的推送可以取消，修改時退回草稿、需要重新送出。對象仍在**送出時**固定。Pulse 0.9.0 就會遵守 `apply_window`，不需要升級 Pulse。主控台 S26：建立表單的「排程與升級時段」、清單與詳情的「已排程」「等待升級時段」。
- **升級推送的自訂批次**（REMOTE.md 第 4.7.1、13.1 節）：`waves` 由最多 2 批放寬為最多 10 批；每批的 `size` 為台數或**當時剩餘**的可升級主機的百分比，最後一批為 `"rest"`，每批各有觀察時間、失敗上限與是否人工繼續。省略 `waves` 時仍是 0.9 的兩批預設範本，既有兩批推送的分批結果不變。預覽另回 `wave_templates`（含第 4.7.1 節建議的四批範本 1% → 10% → 50% → 其餘）與 `max_waves`。主控台 S26 的建立表單可以增減「中間的批次」並套用四批範本，詳情的批次時間軸支援多批。
- **升級推送的雙人覆核**（REMOTE.md 第 5.4、13.1 節）：`kairos.toml` 新增 `[remote] rollout_approval = "none" | "required"`（預設 `none`，需要重新啟動，只能在設定檔改）。`required` 時送出的推送先進入新的狀態 `pending_approval`（對象已固定、不提議任何主機），必須由**建立者與送出者以外**、有新權限 `rollouts:approve`（只有管理員）的使用者以帳號登入的工作階段核准（`POST /api/v1/rollouts/{id}/approve`）才會開始或排程；API token 不能核准（403 `session_required`），建立者與送出者不能核准（403 `self_approval`，`KRS-E6106`）——甲建立、乙送出時需要第三位管理員，只有兩位管理員的部署請由同一個人建立並送出。`required` 時**送出推送（submit）本身也必須是以帳號登入的工作階段**（純 token／CLI 送出 403 `session_required`；建立草稿仍可用 token／CLI），否則單一管理員能用自己的 token 送出推送、再用自己的工作階段核准。建立者與送出者的身分比對以**背後的使用者**為準，不只是稽核執行者字串：`token:<id>` 展開成 token 的擁有者（`owner_user_id`）與建立 token 的人（含已撤銷／過期的 token），`user:<帳號>` 以 `user_id` 比對，因此無法用自己擁有的 token 繞過；`cli:<OS 帳號>` 沒有對應的 Kairos 使用者，由任何合格的管理員工作階段核准。`…/reject`（原因必填）或修改待核准的推送會退回草稿，需要重新送出與核准。稽核 `rollout.approve`、`rollout.reject`；推送另記錄 `submitted_at`／`submitted_by`、`approved_at`／`approved_by`、`reject_reason`（**schema v11**，兩個後端）。降版推送（L3）是固定字串＋原因，不要求重新驗證密碼。
- **核准是伺服器強制的 L2**（ADR-0002 切片 S1；REMOTE.md 第 5.4、13.1 節、API.md 第 17.3 節）：`approve` 的本文必須帶 `confirm`（「核准 N 台」；含降版時 L3 字串「核准降版 N 台」＋`reason`）與 `digest`。待核准的推送有 `submission` 物件——`digest`（這次送審內容的 SHA-256：推送的內容、已固定的主機清單、建立者、送出的時間與送出者）、`confirm_text`、各分類與每批的台數、風險提醒 `warnings[]`、`approvers_available`——與 `viewer`（這次請求的身分能否核准，由後端計算）。伺服器在核准的**同一個交易**中重新計算確認字串與 `digest` 並比對：空的本文或字串不符 400 `confirmation_required`；檢視之後推送被修改或重新送出 409 `submission_changed`；任何一項不通過時狀態不變。稽核 `rollout.submit`、`rollout.approve` 記下 `digest`。**在這之前的開發版本可以用空的本文直接呼叫 API 核准；雙人覆核從未發佈，沒有相容問題。** 同時對同一個待核准的推送核准、退回、修改、取消時只有一個成功（兩個後端都有測試）。清單的 `meta` 新增 `pending_approval_actionable`（這次請求的身分可以核准的筆數）。
- **版本庫檔案的完整性**（ADR-0002 切片 S2；REMOTE.md 第 4.7.2、13.1 節）：資料庫記為已存入、但檔案不存在或大小不符的發佈檔（`KRS-W8417`；只 `stat`）不再被當成可以提議的檔案。**預覽與送出**：需要它的主機分類為新的原因 `artifact_missing`（與 `no_artifact` 分開），不算可升級，預覽另回 `artifact_problems[]`；**核准**：在核准的交易中再檢查一次，檔案有問題時 409 `artifact_missing`（`details.files[]`），狀態不變；**提議**：推送引擎不對需要那個檔案的主機提議、已提議而還沒下載完的收回，主機留在 `pending`（`reason: artifact_missing`），不計入失敗、不自動暫停，補檔後下一輪自動繼續，推送的 `next_action` 為新的 `waiting_artifact{hosts}`。使用中的版本（含待核准與已排程的推送）不能刪除。
- **雙人覆核的政策有紀錄**（ADR-0002 切片 S3；REMOTE.md 第 5.4 節）：`rollout_approval` 仍然只能在設定檔修改——**能修改 Kairos 主機設定或直接寫入資料庫的人可以獨自關閉它（或直接繞過），這是已接受的治理缺口**；主控台內的雙人覆核約束的是主控台帳號（0.10.0 修正了用自己擁有的 API token 送出再自行核准的繞過路徑，見上）。另一個固有邊界：**有 `users:write` 的管理員可以自建帳號、或重設其他管理員的密碼／MFA 後以其身分核准自己的推送**——帳號式雙人覆核假設帳號各由不同的人掌握，不防這種情況，請把 `user.*`、`token.*` 的稽核外送監看。下述的紀錄與告警偵測的是**經由設定檔的關閉**，而且只有在告警即時送到通知管道、稽核即時外送到 Kairos 以外的系統時才可靠（前值記錄在同一個資料庫；這條系統規則可以被維運者以上的帳號預先靜音或停用）。Kairos 把上一次的值保存在資料庫（`meta` 表，不需要 schema 變更），啟動時與目前的值比對，在遠端管理開始接受請求之前寫入稽核 `settings.rollout_approval.baseline`／`change`（執行者 `system`）；寫入失敗時遠端管理不啟動（503 `remote_unavailable`，其餘功能照常）。`required` → `none` 另開新的系統規則「雙人覆核已關閉」（`sys.rollout_approval_disabled`，警告、預設推播、不會自己結束）；系統健康、S26 清單在 30 天內顯示關閉的時間與來源。升級到這一版後第一次啟動只記錄基準，不告警（但目前是 `none` 而資料庫裡有待核准或核准過的推送時，視同關閉）。部署端的補償措施見 `kairos/packaging/INSTALL.md`。
- **主控台**（ADR-0002 S0～S3；設計包尚無這些畫面的定稿，文案為暫行）：S26 清單的「待核准」檢視（可升級台數、送出者、已等待多久、風險標示、「我可以核准」）；側邊導覽的計數泡泡只計目前使用者可以核准的筆數，可存取名稱寫出組成；推送詳情依身分顯示（可以核准的人看到「核准…」「退回…」，建立者與送出者看到誰可以核准）、送審摘要與 `digest`、覆核紀錄、「複製連結」、匯出完整的主機清單（CSV）；核准對話框內嵌送審摘要並要求輸入伺服器算出的確認字串；沒有可核准者時在預覽、送出對話框、詳情頁與系統健康警告。S25 頁首的 `KRS-W8417` 三段式與「重新上傳…」；建立推送的資格預覽列出 `artifact_missing` 與風險提醒；推送詳情與清單的「版本庫的檔案遺失，N 台主機等待中」。S21「連線與安全」新增唯讀的「遠端管理」卡片（雙人覆核：需要／不需要、來源、上一次改變）。不認得的資格原因顯示為「其他原因（代碼）」。全域搜尋的提示文字改為依目前使用者可以搜尋的類別組成（頂列只寫「搜尋」）。
- **外部 URL 的 Pulse 版本庫**（REMOTE.md 第 4.1.2、13.1、17.7 節）：發佈檔可以放在客戶自己的 HTTPS 伺服器，Kairos 只保存 `SHA256SUMS`、簽章與清單。`POST /api/v1/releases` 的 `source: "external"`＋`base_url`、`kairos releases import <目錄> --external <base_url>`、主控台匯入對話框的「外部 URL（選填）」。簽章與雜湊的驗證與代管相同；要推送的發佈檔照樣上傳（或由 CLI 讀取）一次以**登記**雜湊與大小，但不保存在 Kairos。提議的 `artifact.source` 為 `url`、`url` 為 `base_url＋檔名`。主機目前版本的發佈是外部 URL 時，只在目標也是外部 URL 時才附回滾檔。
- **升級中主機的離線告警抑制**（REMOTE.md 第 4.7.5、13.1 節、ALERTS.md 第 5.5 節）：見破壞性變更第 2 項。超過期限後仍離線的事件在 `details.upgrade` 與處理紀錄的新動作 `upgrade_suppressed` 註明「升級中」；Kairos 重新啟動（或叢集換手）後抑制狀態由資料庫重建。
- **SSE `rollout.host`**（REMOTE.md 第 8.4 節、API.md 第 17.5 節）：主控台的 SSE 連線以 `GET /api/v1/events?rollout_id=<推送>` 訂閱一個推送後，推送中每台主機的狀態、進度、代碼改變時收到 `rollout.host`（內容同主機表的一列；需要 `rollouts:read`，沒有訂閱或沒有權限的連線不會收到）。每台主機每一輪（最遲 5 秒）最多一則；一輪改變超過 200 台時改在 `rollout.progress` 帶 `hosts_resync: true`。主控台 S26 的推送詳情改為就地更新主機列，不再每次重新抓取主機表。
- **系統健康的「版本庫的檔案遺失」**（設計包 S20「遠端管理」列）：已存入的發佈檔不存在或大小與記錄不符時（例如還原備份之後：`kairos db backup` 不含發佈檔；或叢集換手後接手的節點沒有匯入過），`GET /api/v1/system/health` 的 `remote` 項目為警告（`KRS-W8417`），`values.release_files_missing`、`missing_files[]` 列出版本、檔名與需要它回滾的主機數；主控台 S20 附「前往 Pulse 版本庫」。重新匯入這個版本的發佈檔即可（大小不符的檔案會被取代）。
- **S04 agent 版本欄的「低於建議版本」**（REMOTE.md 第 4.9 節、R9）：`GET /api/v1/hosts` 新增 `meta.recommended_min_agent_version`（Pulse 版本庫中最新版本的發佈清單的 `min_upgrade_from`；沒有時 `null`）與每一列的 `agent_outdated`，主控台對低於它的主機加 ⚠ 與說明（無法遠端升級到最新版本，需要手動升級；協定仍相容）。只需要 `hosts:read`。
- **全域搜尋納入升級推送與設定檔**：`GET /api/v1/rollouts`、`GET /api/v1/config/profiles` 新增查詢參數 `q`（推送的名稱、編號、目標版本；設定檔的名稱、說明；子字串、不分大小寫），主控台的全域搜尋（Ctrl+K）多了「升級推送」「設定檔」兩組，各需要 `rollouts:read`、`config:read`。

### Pulse

- **外部 URL 的下載**（REMOTE.md 第 17.7 節）：`[upgrade] url_allowlist`（0.9.0 已可設定，現在生效；**預設空＝只從 Kairos 下載**，行為不變）與 `external_ca`。提議的 URL 必須符合白名單的目錄前綴且指向提議的檔名，否則 `declined`（`PLS-W8411`，`url_not_allowed`）；以 HTTPS 下載（TLS 1.2 以上、不跟隨重新導向、大小以提議為上限、斷線以 `Range` 續傳、沒有代理）。**驗證鏈不變**：下載的檔案與從 Kairos 下載的一樣，要符合本機信任錨驗證過的 `SHA256SUMS`，輔助程式以 root 再驗證一次。
- **升級輔助程式的交易紀錄**（ADR-0003 階段 0b；REMOTE.md 第 17.9 節；對 Kairos 與協定沒有影響，`upgrade_status` 的狀態與代碼不變）。root 的輔助程式（`pulse upgrade apply`）把每一次升級當成一筆交易，記在只有 root 可寫的 `/var/lib/pulse-upgrade/<upgrade_id>/state.json`：
  - **正式的 schema**（`pulse.upgrade-helper/v2`）：整份檔案的 `checksum`；替換前實際的版本與 commit、偵測到的安裝方式、目標；替換前後執行檔、驗證過的發佈檔與回滾套件的 SHA-256 與簽署金鑰的指紋；tar 的 unit 新舊雜湊；替換前後套件資料庫的狀態；回滾的意圖。這些「綁定」欄位的雜湊同時寫進進行中交易的指標 `active.json`，接續時兩者必須相同。
  - **明確的步驟**：`preparing` → `prepared` → （寫入指標）→ `replacing` → `replaced` → `rollback_intent` → `rolling_back` → `done`。紀錄先於指標，指標先於任何變更；每一步先寫入並 fsync（檔案與目錄）才做下一步。主機在**任何一點**被終止後，開機時依紀錄與磁碟上的實況（執行檔、`pulse.prev`、unit 的雜湊，套件資料庫）冪等地接續，只使用 root 工作目錄裡驗證過的複本——不重讀服務帳號可寫的暫存區、不重新驗證簽章，`request.json` 只用來觸發。22 個當機點各自的結果列在 REMOTE.md 第 17.9.4 節；任何一點接續之後，`pulse.prev` 都是替換前的版本。
  - **替換無法完成時不再留下替換到一半的系統**：系統還沒被改過就 `failed`（`PLS-E8406`），已被改過就換回 `pulse.prev`（tar 連同 unit）並確認舊版恢復，結果是 `failed`（`detail` 以 `restored` 結尾；deb／rpm 的套件資料庫停在半安裝時附 `package_db_shows_<狀態>`）或 `rollback_failed`（`PLS-E8408`）。先前在替換途中當機後接續時，重新驗證失敗會以 `failed` 結束而不回滾。
  - **紀錄或指標損毀、遺失時不猜**：凡是不確定是否已經動過手，一律不替換、不回滾、不當成全新的請求——`failed`（`PLS-E8402`，`state_unusable`／`transaction_pointer_unusable`，`detail` 以 `manual check required` 結尾），現場保留。處理方式：檢查主機後刪除 `/var/lib/pulse-upgrade/active.json` 與對應的工作目錄，才能再遠端升級。
  - **路徑檢查**（每次執行、讀取指標之前）：`/var/lib/pulse-upgrade` 與每筆交易的工作目錄必須是 root 擁有、不是符號連結、群組與其他人沒有任何權限（只多了讀取／執行權限時改回 0700 並記 `WARN`）；它們與 `/opt/pulse` 的每一個上層目錄必須由 root 擁有、群組與其他人不可寫（sticky 的目錄除外）；`/opt/pulse/pulse` 必須是 root 擁有的一般檔案，`pulse.prev` 不得是符號連結。不通過：`failed`（`PLS-E8405`，`unsafe_path: <路徑>: <原因>`），不做任何變更。**依 INSTALL 安裝的主機不受影響**（`/opt/pulse` 為 `root:pulse 751`）；自行把 `/opt/pulse` 或它的上層目錄改成群組可寫、或把 `/opt/pulse` 做成符號連結的主機，遠端升級會被拒絕。
  - **工作目錄不隨請求的次數成長**：同一時間只有一筆交易；只保留最近 1 筆已結束的紀錄與最近 3 筆 `rollback_failed`（只有最近的一筆保留全部檔案），已結束的交易只留 `state.json` 與 `upgrade.log`。
  - 由 0.9.0 的輔助程式開始的交易（`state.json` 為 v1；也就是 0.9.0 → 0.10.0 的那一次升級途中重新開機）：只接續已替換之後的步驟，轉成 v2 後依上面的規則；v1 的 `verified` 以輔助程式自己的版本判斷是否已替換，無法判斷時 `state_unusable`。
- **hello 事先回報路徑檢查的結果：`install.upgrade = "unsafe_path"`**（REMOTE.md 第 2.1、17.9.7 節）。其他條件都就緒的主機，Pulse 在每次連線前以**與升級輔助程式相同的函式**檢查 `/opt/pulse`、`/opt/pulse/pulse`、`pulse.prev`、`/var/lib/pulse-upgrade` 與它們的上層目錄；不通過時回報 `unsafe_path`、不宣告 `upgrade`，`pulse upgrade status` 顯示「路徑不安全（`PLS-E8405`）」與 `unsafe_path: <路徑>: <原因>`。Kairos 的資格分類新增原因 `unsafe_path`（主控台的資格預覽、推送中主機的原因、主機詳情的「升級」卡片與主機清單的「遠端升級」篩選都顯示「路徑不安全」與處置方式），不合規的主機在建立推送時就被標出來，而不是等推送以 `PLS-E8405` 失敗。預檢由服務帳號執行、不建立也不修改任何東西：看不到的部分（權限不足）略過、不誤報，每筆交易的工作目錄（`/var/lib/pulse-upgrade/<upgrade_id>`）不在預檢的範圍；**輔助程式自己的檢查不變**，執行時仍會再檢查一次。**協定版本與 schema 不變**：`install` 的值本來就允許不認得的——0.9 的 Kairos 收到 `unsafe_path` 時原樣保存、分類為 `not_ready`（主控台顯示「其他」）；0.9 的 Pulse 不做預檢（它的輔助程式也沒有路徑檢查）。路徑在連線期間被改掉時，要到下一次重新連線才會回報。
- 其餘沒有變更：0.10.0 沒有新的協定 feature；叢集模式的容錯移轉使用 0.9.0 已有的 `server.fallback_addresses` 與 feature `cluster`。

### 安全性

- **Pulse：取得服務帳號的人可以在升級輔助程式以 root 執行期間，把請求目錄換成符號連結，讓 root 在任意目錄寫檔或刪檔（本機提權）**（**0.9.0 受影響**；REMOTE.md 第 17.9.7 節「請求目錄」、第 17.2 節 #15）。請求目錄 `/var/lib/pulse/upgrade` 由低權限的 `pulse` 服務帳號擁有、可寫；0.9.0 的輔助程式以字面路徑推導這個目錄下的 `request.json`／`result.json`／暫存子目錄，沒有在執行期間防範該目錄被改名、換成指向任意目錄 D 的符號連結（`O_EXCL | O_NOFOLLOW` 只保護路徑的最後一段，上層目錄被換成符號連結仍會被跟隨）。已取得 `pulse` 帳號的人可以在輔助程式複製檔案或等待寫結果時換掉該目錄，使 root 在 D 建立／覆寫 `result.json`（0644）、刪除 `D/request.json`、遞迴刪除 `D/<upgrade_id>`；餵一個會被拒絕的請求即可觸發，不必等一次成功的升級。再配合 0.9.0 不檢查 `request.json` 的 `target.version`／`target.commit`、`result.json` 的 `to_version` 原樣寫入且不跳脫，可把 `$( ) \` 反引號、換行等字元注入落在會被 root 互動登入時 `source` 的目錄（例如 bash-completion）而取得 root。攻擊者必須已經取得 `pulse` 帳號。**修正**：輔助程式一開始就以 `O_DIRECTORY | O_NOFOLLOW` 開啟請求目錄取得一個 fd、`fstat` 確認是目錄且擁有者合理，之後對該目錄的所有操作——讀 `request.json`、寫 `result.json`、刪 `request.json`、遞迴刪暫存子目錄、讀暫存的發佈檔——全部相對這個 fd 以 `openat`／`renameat`／`unlinkat` 進行（遞迴刪除全程 `AT_SYMLINK_NOFOLLOW`／`O_NOFOLLOW`）；fd 綁定開啟當下的 inode，執行期間換掉路徑也改變不了 root 寫到哪裡、刪掉哪裡（TOCTOU）。請求目錄本身在開啟時若已是符號連結或不是目錄，輔助程式什麼都不碰、不寫結果就結束。另外 `Request::validate` 以 `release::Version::parse` 限制 `target.version`、以「7..=40 個小寫十六進位」限制 `target.commit`，不合即拒；寫 `result.json` 的 `to_version`／`from_version` 前再套一次 `sanitize_detail` 與長度上限。以 `libc` 的 `*at` 系列實作（`remote/upgrade/helper/reqdir.rs`），**沒有新增相依**。`/opt/pulse`、`/var/lib/pulse-upgrade` 的既有檢查不變。**0.9.0 的主機升級到 0.10.0 的那一次（遠端升級）仍由 0.9.0 的輔助程式執行，那一次還沒有保護**，之後的升級才受到保護——在意這個風險的主機請手動升級到 0.10.0。**只在 0.10.0 修正，0.9.x 不另外發佈修正版**（0.9.0 還沒有正式部署的使用者）。系統層測試 `tests/systemd` 的輔助程式情境涵蓋修正後的行為；單元測試（`helper/fault_tests.rs`）斷言請求目錄是符號連結時被拒、執行期間換掉也不會把 root 的寫入導向連結目標、危險的 `target` 被拒且不寫入結果。
- **Pulse：取得服務帳號的人可以藉升級輔助程式的「回滾」把主機降到任一個有官方簽章的舊版**（**0.9.0 受影響**；deb／rpm 安裝；REMOTE.md 第 5.1、17.8 節）。root 的輔助程式（`pulse upgrade apply`）驗證回滾檔時，預期的版本取自 `request.json` 的 `from_version`，而 `request.json` 是 `pulse` 服務帳號寫的。已取得 `pulse` 帳號的人可以自己寫一份請求——目標是較新的官方版本，`from_version` 謊報為較舊的版本並附上那個舊版有官方簽章的套件當回滾檔，再讓新版的健康確認失敗——輔助程式就會以 `dpkg -i`／`rpm -U --oldpackage` 裝回那個舊版，**繞過本機的 `upgrade.allow_downgrade = false`**。攻擊者必須已經取得 `pulse` 帳號，能安裝的只限有官方簽章的 Pulse 套件；tar 安裝不受影響。**修正**：輔助程式改以自己的版本（＝目前安裝的版本）為準，`from_version` 不符就拒絕（`failed`、`PLS-E8402`、`from_version_mismatch`，什麼都不改）；安裝方式也改由輔助程式自己偵測（請求的 `method` 不符就拒絕，`method_mismatch`）；替換前把版本、安裝方式、目標與驗證過的回滾檔名寫進只有 root 可寫的 `state.json`，重新開機後接續時只以它為準，不再採用 `request.json` 的這些欄位（0.9.0 每次執行都重讀 `request.json`，服務帳號可以在替換後改寫）；`from_version` 必須是合法的版本號。工作目錄也不再由 `request.json` 的 `upgrade_id` 決定：輔助程式在 root 專用的 `/var/lib/pulse-upgrade/active.json` 記下進行中的交易，有尚未結束的交易時只接續那一筆（請求被換成另一個 `upgrade_id` 時不開新的交易，結果加註 `request_upgrade_id_ignored`）；狀態檔與這個指標寫入後另外 fsync 目錄。狀態檔 fail closed：替換前寫不進 `state.json` 就不開始替換（`state_not_saved`；0.9.0 只記 `WARN`），狀態檔損毀或遺失而這次升級已經動過手時不再當成全新的請求（`state_unusable`，需要人工檢查；0.9.0 會重新開始並改信 `request.json`）。正常的升級不受影響（Pulse 一向以自己的版本填 `from_version`）。**0.9.0 的主機升級到 0.10.0 的那一次（遠端升級）仍由 0.9.0 的輔助程式執行，那一次還沒有保護**，之後的升級才受到保護——在意這個風險的主機請手動升級（`apt install`／`dnf install`／換執行檔）到 0.10.0；不使用遠端升級的主機可在 `pulse.toml` 設定 `[upgrade] enabled = false`。**這個問題只在 0.10.0 修正，0.9.x 不另外發佈修正版**（0.9.0 還沒有正式部署的使用者）；使用 0.9.0 的主機請升級到 0.10.0。系統層測試 `tests/systemd/run.sh` 新增情境 `tamper`（Debian 12 的 deb 與 Rocky 9 的 rpm：以服務帳號寫入竄改的 `request.json`，輔助程式拒絕、版本與套件資料庫不變）。上述狀態檔在發佈的 0.10.0 中是有 checksum 與綁定雜湊的正式交易紀錄，另有路徑元件的擁有者、權限與符號連結檢查、工作目錄數量的上限與逐一當機點的故障注入測試（「Pulse」一節的「升級輔助程式的交易紀錄」；ADR-0003 階段 0b）：`pulse` 服務帳號無法以大量的 `upgrade_id` 或反覆失敗的升級塞滿 `/var/lib/pulse-upgrade`，也無法藉既存的、權限過寬或被換成符號連結的工作目錄影響 root 的輔助程式。

### 修正

- **主控台 S25：版本庫的檔案遺失後無法從主控台補檔**（0.10.0 開發期間發現，未曾發佈）：系統健康提示「版本庫的檔案遺失」後，在 S25 重新匯入同一個版本時，資料庫標為已上傳的發佈檔被顯示為「已上傳過」並跳過，實體檔已遺失也一樣（只有 `kairos releases import` 會補上）。`GET /api/v1/releases`、`GET /api/v1/releases/{version}` 的 `artifacts[]` 新增 `file_problem`（`missing`／`size_mismatch`／`null`）；S25 的清單與詳情標出「檔案遺失（請重新上傳）」「檔案大小不符（請重新上傳）」，詳情的「上傳發佈檔…」在有這類檔案時出現，重新上傳時照常上傳並補上。
- **Pulse：升級在「替換完成、還沒記下已替換」時中斷，接續後回滾換不回舊版**（0.9.0；REMOTE.md 第 17.8 節）：接續時輔助程式再替換一次，而替換一開始就覆寫 `pulse.prev`——此時目前的執行檔已是新版，舊版的備份被蓋掉，之後健康確認失敗時回滾換回的仍是新版，最後回報 `rollback_failed`（tar，以及沒有回滾檔的 deb／rpm）。現在狀態檔記下這次升級是否已保留 `pulse.prev`，已保留就不再覆寫；替換的每個動作在接續時都先看磁碟上的實況（執行檔、`pulse.prev`、unit 的雜湊）再決定要不要做，tar 在這個窗口已換上的新 unit 回滾時也會還原（REMOTE.md 第 17.9.4 節第 11、12 列）。
- **Pulse：升級輔助程式自己被終止（OOM、人工 `kill` 等）而主機沒有重新開機時，接續後誤判健康確認**（0.9.0；REMOTE.md 第 17.9.3 節）：輔助程式在記下「已替換」之後、送出 `systemctl restart pulse` 之前被終止的話，接續時直接做健康確認——服務仍是替換前啟動的舊行程，一路等到期限後回滾一個其實沒有問題的升級；回滾時同樣的窗口則是 `rollback_failed`。現在交易紀錄記下重新啟動是否已送出，接續時還沒送出就補送。
- **`kairos reports export` 在 Kairos 執行中可能誤報「report 內容損毀」**（`KRS-E4301`，結束代碼 1；0.3.0 起就有，兩個儲存後端都是）：匯出先讀序列對照表、再讀 report，兩次讀取各自的快照；剛好有新的主機或新的指標出現（Kairos 在同一個交易中建立序列並寫入參照它的 report）時，後讀到的 report 參照對照表中沒有的序列而被誤判。資料沒有損毀，重新執行就會成功。現在兩者在同一個快照（SQLite：同一個讀取交易；PostgreSQL：`REPEATABLE READ` 的唯讀交易）中讀取。整合時由叢集的端對端測試發現（三次中出現一次）。

### 已知限制

- **PostgreSQL 後端的寫入效能目標尚未達成**（HA.md 第 7.6 節；量測見 STORAGE.md 第 19.8 節）：目標是每秒 2,000 筆時 `ack` 的 p99 < 50 ms、每秒 8,000 筆的補送尖峰 5 分鐘沒有 `storage_unavailable`。在 4 vCPU、4 GB RAM 的量測環境（PostgreSQL 在同一台機器、記憶體上限 512 MB 的容器中）：穩定狀態每秒 2,000 筆的 p99 為 41 ms、每秒 8,000 筆為 85 ms（SQLite 約 12 ms）；**1 萬台全新的主機同時出現時前 15 秒左右的寫入會超過 5 秒**（回 `storage_unavailable`，Pulse 重送，不遺失）；**每秒 8,000 筆的補送尖峰撐不住**（60 秒內 26% 回 `storage_unavailable`）。這不是目標環境，**必須在正式規格的硬體與獨立的資料庫主機上重新量測**後才能對外承諾數字。PostgreSQL 上的容量估算請乘以約 1.6。
- **叢集模式是階段 A（主動／備援）的 HA 預覽**：只有一個節點服務連線；階段 B（多主動連線層、跨節點 `superseded`、DNS SRV）沒有做。**沒有驗證的故障情境**（HA.md 第 17.1 節）：主動節點主機斷電或網路線被拔（F2）、主動節點與資料庫之間真正的網路中斷（F3，只有模擬）、PostgreSQL 主庫故障後自動切換到備庫（F5）、LB 或虛擬 IP 的主機故障（F7）、節點之間的網路中斷（F8，設計上無影響）、節點的系統時鐘被調偏（F10）、兩台主機誤用同一個 `node_id`（F13）、混用兩個版本的滾動升級（F14）、兩個以上備援節點同時搶租約的端對端情境、1,000 台以上主機的接手時間、PgBouncer 之後的連線、Windows 上的 Kairos。量測的數字來自一台 Pulse、同一台主機的 loopback。
- **PROXY protocol v2 沒有做**（`KRS-W2206` 尚未使用）：經 L4 負載平衡器時，每 IP 的連線與速率限制、日誌與稽核看到的來源都是負載平衡器的位址。請把負載平衡器的位址加入允許清單，或改用虛擬 IP／`server.fallback_addresses`。
- **遠端升級的版本庫是節點本機的檔案**（`<data_dir>/releases/`）：版本與發佈檔的清單在共用的資料庫，檔案只在匯入它的那個節點。叢集部署要**在每個節點各自匯入相同的發佈版本**（`kairos releases import` 可以在備援節點執行；主控台的上傳只存到當時的主動節點），否則換手後進行中的推送會下載失敗；接手的節點以系統健康的「版本庫的檔案遺失」（`KRS-W8417`）標出它缺少的檔案，需要這些檔案的主機留在未開始、補檔後自動繼續。刪除版本只刪除執行它的節點上的檔案。外部 URL 的版本庫不受影響。跨節點同步是待決事項。
- **Windows 的遠端升級不含在 0.10.0**（REMOTE.md 第 4.4.4、18 節）：Windows 主機的 `install.upgrade` 仍是 `unsupported`（推送的資格分類為 `unsupported_platform`），以既有的 MSI／SCCM 升級；集中設定不受影響。理由：升級程序的每一步（脫離服務、`msiexec`、服務控制、ACL 檢查、回滾）都只能在 Windows 實機驗證，而信任邊界（ACL）寫錯不會有任何錯誤訊息；0.8.0 的 Windows 服務與 MSI 本身也還沒有實機驗證。需要的 VM 與驗證清單見 REMOTE.md 第 18.3 節，是否與何時實作待決定。
- **升級時段的時區是固定位移**（例如 `+08:00`；不支援 `Asia/Taipei` 這類時區名稱）：有日光節約時間的地區，換季時要自行修改推送的時段。叢集模式下排程與時段以主動節點的系統時鐘判斷，節點之間的時差會讓換手後的時間邊界偏移同樣的量（時差超過 2 秒記 `KRS-W8305`）。
- **暫定的產品代碼**：`KRS-E4406`（這個命令不支援 PostgreSQL 後端）尚待設計包正式分配，正式分配後代碼可能改變；請不要在自動化中寫死這個代碼。`KRS-W8417`（版本庫的檔案遺失）已決定照這個號碼採用（ADR-0002 U10），但設計包的總表與三段式文案尚未收錄，主控台的文案是暫行版本。系統規則「雙人覆核已關閉」還沒有產品代碼。
- 叢集模式還沒有：主控台的叢集畫面與排空／交還的 API、叢集相關的系統告警規則（節點失聯等只顯示在系統健康、`kairos cluster status` 與日誌）、通知的 `delivery_id`（換手時通知是 at-least-once）。備援節點的 `/metrics` 只有版本與 `kairos_cluster_*`。
- **Pulse 升級輔助程式：回滾檔的信任與套件回滾失敗的處理留待之後的版本**（ADR-0003 階段 1、2；REMOTE.md 第 17.5、17.8 節）：套件回滾失敗（`dpkg -i`／`rpm -U` 回報錯誤）時直接 `rollback_failed`，不會退回以 `pulse.prev` 換回執行檔；第一次遠端升級沒有可信的回滾套件（只換回執行檔，套件資料庫仍顯示失敗的版本）；沒有 `rollback_bundle` 與本機封存。交易紀錄或進行中交易的指標損毀時輔助程式拒絕繼續（`state_unusable`／`transaction_pointer_unusable`），需要人工檢查主機後刪除 `/var/lib/pulse-upgrade/active.json` 與對應的工作目錄才能再遠端升級。路徑檢查不含掛載選項（`nosuid`、`noexec`）、ACL 與 SELinux 標籤。**真正的斷電沒有實測**：當機點的測試（單元測試的 22 個當機點、系統層的 2 個）終止的是行程，已寫入頁面快取的資料不會遺失；fsync 的順序由程式審查與單元測試中每個當機點的順序斷言保證。
- **雙人覆核的範圍**（ADR-0002 只實作安全的最小組合 S0～S3）：只涵蓋升級推送的送出；`rollout_approval` 只能在設定檔修改，**能修改 Kairos 主機設定或直接寫入資料庫的人**可以獨自關閉它（或直接繞過）（Kairos 只做到稽核與告警，而且需要外送才可靠，請搭配部署端的流程）；推送清單只有「全部」與「待核准」兩個檢視，其他狀態用狀態篩選；**沒有待核准的逾時與通知規則**（待核准的推送會一直佔住它的主機，主控台只顯示已等待多久與「複製連結」）；建立推送仍是單頁表單，**被退回的推送不能在主控台修改**（取消後重新建立，或以 API 的 `PUT` 修改；`PUT` 沒有樂觀鎖）；批次與升級時段的編輯器維持現況；甲建立、乙送出的推送需要第三位管理員核准。版本庫檔案的檢查只 `stat`（存在與大小），不重算雜湊。
- 降版推送（L3）沒有重新驗證密碼。Kairos 無法在預覽時得知哪些主機允許外部 URL。
- PostgreSQL 後端沒有量測的：`kairos db migrate` 的速度、同步備庫下的延遲、1 萬台以上的規模、長時間的行為、PostgreSQL 17 的效能（只跑過功能測試）。

### 其他變更

- 新依賴：`postgres` 0.19、`tokio-postgres-rustls` 0.14（MIT／Apache-2.0；`deny.toml` 的授權允許清單不需變更，加密後端仍只有 ring）。`tokio` 升級到 1.53.3（RUSTSEC-2026-0386）、`rusqlite` 升級到 0.41（內附 SQLite 3.54，RUSTSEC-2026-0459）。Pulse 的 `tokio` 另啟用 `fs`。
- **測試用的 Cargo feature `fault-injection`（Pulse）與發佈檔的檢查**（REMOTE.md 第 17.9.9 節）：`tests/systemd/` 的實機當機點需要讓輔助程式在指定的點停住，這段程式只在開啟 feature 的「測試用建置」中（`pulse --version` 結尾另有 `[fault-injection]`），**正式發佈的執行檔不含**。打包：`packaging/build-release-common.sh` 只在環境變數 `TEST_FEATURES=fault-injection` 時開啟它（只有 `tests/systemd/build.sh` 這樣做），否則每個目標建置後以新的 `packaging/check-release-binary.sh` 確認執行檔不含故障注入的標記，有就中止打包；`packaging/release.sh` 在環境中有 `TEST_FEATURES` 時拒絕執行，並在簽章前對 `tar.gz` 內的執行檔再檢查一次；CI 的 package job 對要上傳的發佈檔再檢查一次，check job 另以 `--features fault-injection` 跑 Pulse 的 clippy。**發佈者不需要改變流程**；`TEST_FEATURES` 不要出現在發佈用的環境。
- 輔助程式的測試：故障注入的單元測試矩陣 `remote::upgrade::helper::fault_tests`（REMOTE.md 第 17.9.4 節對照表的 22 列 × tar／deb（有、無回滾套件）／rpm × 新版健康／崩潰，每一個當機點都斷言當機時磁碟上的狀態與接續後的結果，並在每個 checkpoint 檢查寫入順序；測試讀文件的對照表，確認矩陣與它一致），另有紀錄與指標損毀或遺失、舊格式、路徑檢查、保留上限。系統層 `tests/systemd/run.sh`：情境 `tamper-resume` 擴充（另竄改 `from_version`；以 `sha256sum` 重算 root 的指標與交易紀錄的 checksum、檢查綁定與權限、結束後的清理），新增 `crash-replace`（替換後、記錄前 `docker kill`）與 `crash-rollback`（還原後、記錄前 `docker kill`）。
- 量測 `sqlite_throughput` 改名為 `store_throughput`，以 `KP_BENCH_BACKEND=postgres` 量測 PostgreSQL 後端。
- 儲存層的測試改為同一個測試本體在 SQLite 與 PostgreSQL 各跑一次（`KAIROS_TEST_PG_URL` 指定資料庫；沒有設定時明確略過，`KAIROS_TEST_PG_REQUIRED=1` 時不可略過）；CI 的 Linux job 以 PostgreSQL 14 執行整套測試，另有 `postgres-compat` job 以 PostgreSQL 17 執行儲存層與 PostgreSQL 的端對端測試。Windows job 不執行 PostgreSQL 的測試。
- 叢集的測試：租約在假時鐘下的取得、續約、到期與交還（兩個後端）、fencing 擋下舊 leader 的每一種寫入、接手等待舊 leader 進行中的交易、多個節點同時搶租約只有一個成功、協調執行緒的接手與自我降級、schema 的升級（SQLite v9 → v11；PostgreSQL v9 → v11 與 v10 → v11）；端對端測試 `tests/e2e/tests/cluster.rs`：兩個實際的 Kairos＋Pulse＋PostgreSQL 的容錯移轉（SIGKILL 主動節點、`step-down`）後 durable 的資料不缺號、不重複、啟動保護，以及遠端管理在換手後的狀態（`remote_upgrade_state_survives_failover`）。
- 端對端測試 `tests/e2e/tests/postgres.rs`：實際的 Kairos＋Pulse 跑在 PostgreSQL 後端上（補送不重複不缺號、重新啟動後載回主機、稽核鏈驗證、`db migrate` 搬回 SQLite 後內容相同）；`tests/e2e/tests/releases_cli.rs`：`kairos releases` 與執行中的 Kairos 並用。

### 新的產品代碼

`kairos/HA.md` 第 19 節已由設計包正式分配、這一版開始使用的代碼：`KRS-E4401`（無法連線到 PostgreSQL）、`KRS-E4402`（PostgreSQL 不符合要求；也用於「schema 已有另一個 Kairos 在使用」）、`KRS-W4403`（沒有同步複寫）、`KRS-E4404`（資料搬移失敗或驗證不一致）、`KRS-E4405`（schema 比這個 Kairos 新）、`KRS-W8305`（時差超過 2 秒），以及叢集的 `KRS-I8501`（取得 leader 租約）、`KRS-W8502`（失去 leader 租約，結束代碼 75）、`KRS-E8503`（沒有 leader，或 `node_id` 重複）、`KRS-E8504`（金鑰與叢集不一致或缺少）、`KRS-W8505`（節點的設定不一致）、`KRS-W8506`（節點失聯）；`KRS-I8507`（排空）另用於 `kairos cluster drain`。遠端管理：`KRS-E6106`（設計包 §31.3 已分配，0.10 起使用）——不能核准自己建立或送出的推送。`KRS-W2206`（PROXY protocol）尚未使用。

| 代碼 | 意義 | 狀態 |
|---|---|---|
| `KRS-E4406` | 這個命令不支援 PostgreSQL 後端（`kairos db backup`、`kairos reports import`） | **暫定**，待設計包正式分配（子類別 44） |
| `KRS-W8417` | 版本庫的檔案遺失（已存入的發佈檔不存在或大小與記錄不符）：系統健康、S25、資格分類 `artifact_missing`、核准時的 409 `artifact_missing`、推送中等待檔案的主機 | 照工程的號碼採用（ADR-0002 U10，子類別 84、只有 Kairos 端）；設計包的總表與三段式尚未收錄 |

## [0.9.0] - 2026-11-11

**Kairos 0.9.0、Pulse 0.9.0**：衝刺 8——**指標轉出**（商業審計 F7，`kairos/EXPORT.md`：Prometheus exporter、OTLP/HTTP 推送、Grafana 儀表板）、**個資保護**（商業審計 S6，`kairos/PRIVACY.md`：`[privacy]`、刪除主機資料、個資匯出、DPA 範本骨架）與衝刺 4 延後的小項（手動簽發憑證的匯入、8444 的下載端點、`pulse enroll` 送出申請前的 429 等候、過期寬限期 #14 的端對端測試），**HA 的 L4**（審計 A2，`kairos/HA.md`：Pulse 的多個伺服器位址 `server.fallback_addresses`、協定 feature `cluster` 與關機排空 `drain`），**集中設定與 Pulse 遠端升級**（F8，`crates/kairos-proto/REMOTE.md`）的 Kairos 端（K1／K2）、Pulse 端（P1／P2）與共用協定（S1），另有高可用的規格。**協定版本不變（v1～v2）**，新增 feature `cluster`、`config`、`upgrade`（雙方都宣告時才使用），8444 的 API 不變，0.8 與 0.9 的任何組合都能連線；Kairos 的資料庫升級到 **schema v9**（不可逆，升級前先備份）。

0.9.0 的範圍（2026-10-19 使用者決定，HA.md 第 21 節、REMOTE.md 第 14 節全部採建議）另包含 **F8 MVP**（REMOTE.md D15；Kairos 端 K1 設定檔與指派、K2 版本庫與推送，以及 Pulse 端 P1 集中設定、P2 Linux 遠端升級都已包含，Kairos 0.9 × Pulse 0.9 在 mTLS 連線上協商 `config`／`upgrade`）與 HA 的 **L1a 儲存抽象**（已包含）、**L4 Pulse 多位址**（已包含）；PostgreSQL 後端與叢集協調延到 0.10.0。

### 破壞性變更與升級步驟（升級前必讀）

| # | 產品 | 變更 | 誰會受影響 | 升級步驟 |
|---|---|---|---|---|
| 1 | Kairos | **行程排行的命令列預設遮蔽秘密**（`privacy.process_command` 預設 `redact`，行為變更）：Pulse 開啟完整命令列（`processes.command = "full"`）的主機，命令列中的密碼、token、帶帳號密碼的網址在保存前遮蔽（與診斷包相同的規則，另加 `--password x`、`-pSECRET`；`-p` 開頭的單一破折號參數一律遮蔽）。主控台、API、匯出、指標轉出都只看到遮蔽後的值 | 在 Pulse 開啟完整命令列、需要原樣檢視命令列的部署（Pulse 預設只送執行檔，不受影響） | 需要原樣保存時在 `kairos.toml` 設定 `[privacy] process_command = "keep"` |
| 2 | Pulse | **`pulse enroll` 在送出申請之前收到 429／503 時等候後重試**（依 `Retry-After`，合計不超過 `--wait`，沒有指定時 600 秒）：0.8 直接結束（結束代碼 6） | 依結束代碼 6 立即自行重試的部署腳本 | 腳本不需要自行重試；需要立即失敗時加 `--no-wait` 或較短的 `--wait <秒>` |
| 3 | Pulse | **存在但讀不到的 `pulse.toml` 是設定錯誤**：服務以 `PLS-E0402`、結束代碼 3 停止（systemd 不重啟）；0.8 當成沒有設定檔、以內建預設值（連 127.0.0.1、不用 mTLS）啟動。`pulse config show`、`pulse upgrade status` 同樣報錯 | 服務帳號讀不到 `pulse.toml` 的主機（例如 0.8 的 `pulse-install.sh`／`pulse enroll` 改寫後變成 `root:root 640`，見下方「Pulse」） | 升級前確認 `ls -l /opt/pulse/pulse.toml` 為 `root:pulse 640`（`chgrp pulse /opt/pulse/pulse.toml`）；`pulse check-config` 以服務帳號執行可提前發現 |

**升級步驟**：Kairos 第一次啟動時把資料庫升級到 **schema v9**（只新增資料表與 `hosts` 的欄位，一個交易；升級後 0.8 的 Kairos 無法開啟，降版從備份還原），請先 `kairos db backup <檔案> --include-keys --password-stdin < <密碼檔>`（0.9 起一併備份 `data/privacy.key`）→ 換上 Kairos 0.9.0（tar.gz 照 INSTALL 第 11 節換執行檔；deb／rpm 直接升級）→ 換上 Pulse 0.9.0。指標轉出與 `[privacy]` 的假名化、網段預設都不啟用；需要時依 EXPORT.md、PRIVACY.md 設定。

**遠端升級的第一次導入**（REMOTE.md 第 4.10 節）：遠端升級需要主機上已有 0.9.0 的 Pulse、輔助程式 `pulse-upgrade.path` 與信任錨 `/etc/kairos-pulse/allowed_signers`，因此**每台主機都要手動升級一次到 0.9.0**（套件：`apt install ./pulse-0.9.0-….deb`／`dnf install` 會安裝並啟用 `pulse-upgrade.path`；一行安裝腳本另外放置信任錨；tar：依 INSTALL.md 第 10.1 節安裝兩個 unit 與信任錨），之後的版本才能由 Kairos 推送。不要遠端升級或集中設定的主機在 `pulse.toml` 設定 `[upgrade] enabled = false`、`[remote_config] enabled = false`（或安裝腳本的 `--no-remote-upgrade`、`--no-remote-config`）；Kairos 端可用 `[remote] config_enabled`／`upgrade_enabled` 整體停用。

### 相容性

- 協定（v1～v2）、8444 的 API 都不變；schema 升級到 v9。8443 新增 feature `cluster`、`config`、`upgrade`（雙方都宣告時才使用），0.8 與 0.9 的 Kairos／Pulse 任意組合都能連線（下表）。`config`／`upgrade` 另需要 Kairos 以 SQLite 儲存、agent 連線埠啟用 mTLS；沒有宣告的 client（Pulse 0.8 以前）收到的 `hello_ack`、`ack` 與 0.8 完全相同（沒有 `pending`），新的 `type` 照舊回 `unknown_type`。**遠端升級需要主機上已經有 0.9.0 的 Pulse、輔助程式與信任錨，0.8 → 0.9 必須手動升級一次**（REMOTE.md 第 4.10 節）。
- 降版到 0.8 的 Pulse 會忽略 `server.fallback_addresses`，只連 `server.address`。
- **注意**：所有候選位址必須屬於共用同一個資料庫的同一個叢集，durable 的去重才是全叢集的。0.9.0 還沒有共用資料庫的叢集模式（PostgreSQL、租約為 0.10.0），把備用位址指到另一個各自有 SQLite 的 Kairos 時，換節點時在途的 report（最多 4 筆）可能兩邊各寫一次。
- 8444（註冊、輪詢、過期續簽）不跟著候選位址走，仍只用 `enroll.address`（空字串時為 `server.address` 的主機）；叢集部署請設為負載平衡器或虛擬 IP 的名稱。DNS SRV（`server.srv`）未實作（HA 規格的階段 B 選用）。
- 新權限 `export:write`、`hosts:purge`、`hosts:personal_data` 只給內建的管理員；F8 的 `config:read`／`write`、`releases:read`／`write`、`rollouts:read`／`write`／`pause` 依 REMOTE.md 第 5.2 節（`Permission::ALL` 為 45 個）；0.4／0.5 的舊範圍 token 不會取得（權限凍結）。維運者看不到 S21「整合」分頁（維持 D16）。
- 預設值下的行為與 0.8 相同，唯一的差異是 `process_command = "redact"`（破壞性變更第 1 項）；Prometheus exporter 與 OTLP 推送預設停用；feature `cluster` 只在 Kairos 0.9 × Pulse 0.9 時使用（節點名稱、Kairos 優雅關機時的 `drain`）。
- 指標轉出的設定存在既有的 `settings` 表，不新增資料表；`[privacy]` 在資料進入時轉換，主控台、API、匯出與指標轉出都只看到轉換後的值。

| Pulse ＼ Kairos | Kairos 0.8（以前） | Kairos 0.9 |
|---|---|---|
| **Pulse 0.8（以前）** | 不變 | 與 0.8 相同：不宣告 `cluster` 的 client 收到的 `hello_ack` 不變，也不會收到 `drain`（Kairos 關機時照舊直接關閉連線） |
| **Pulse 0.9** | 舊版 Kairos 不列出 `cluster`、`config`、`upgrade`：沒有節點名稱、不會收到 `drain`、只用本機設定、不會被升級；`fallback_addresses` 照常運作 | 協商 `cluster`：節點名稱、關機排空；mTLS 時另協商 `config`、`upgrade`（集中設定、遠端升級） |

### Kairos：指標轉出（F7；`kairos/EXPORT.md`）

- **Prometheus exporter**：`GET /api/v1/export/prometheus`（主控台的監聽，HTTPS）以 Prometheus text 0.0.4 或 OpenMetrics 1.0（依 `Accept`）輸出所有可見主機的最新指標。以 API token 驗證、權限 `hosts:read`（viewer 角色即可；群組範圍的使用者只看得到自己的群組，另可 `?group=` 分群抓取）。每個序列帶 `agent_id`、`hostname`、`host_group`；另有 `kairos_host_up`、`kairos_host_info`（顯示名稱、版本、作業系統、標籤）、`kairos_host_last_sample_timestamp_seconds`、`kairos_exporter_hosts`。離線或最後一筆超過 `stale_after_secs`（預設 300）的主機不輸出指標；行程排行預設不輸出（`include_processes`）。預設**停用**（`[export.prometheus] enabled`，未啟用時 `404 export_disabled`）。
- **OTLP/HTTP 推送**：`[export.otlp]` 把每一筆保存成功的 report 以 protobuf 推送到 OpenTelemetry collector：批次（`max_batch_points`）、有上限的佇列與丟棄策略（`queue_max_points`、`drop_policy`）、重試（429／502／503／504 與連線失敗，指數退避、`Retry-After`）、TLS（作業系統的 CA＋`ca_file`、可帶用戶端憑證）、自訂標頭（`headers_file` 或主控台輸入，加密保存）、resource 屬性（`host.name`、`host.id`、`host.arch`、`os.type`、`kairos.host.*`、`resource_attributes`）。名稱預設對應 OpenTelemetry semantic conventions（`naming = "semconv"`：`system.cpu.utilization`、`system.memory.usage{system.memory.state}`、`system.network.io{network.io.direction}`…），可改回 Kairos 的名稱。不支援 gRPC 與 gzip。預設停用。
- **Grafana**：儀表板 `kairos/packaging/grafana-kairos-hosts.json`（Prometheus 資料來源；群組與主機變數、CPU、記憶體、負載、磁碟、網路、運作時間、主機清單）與抓取範例 `kairos/packaging/prometheus-export.example.yml`，兩者與 `EXPORT.md` 一起放進發佈包。
- **主控台**：S21「系統設定 → 整合」新增「指標轉出」卡片：Prometheus 的最近一次抓取、OTLP 的佇列／送出／丟棄／最近一次成功與錯誤；管理員可修改啟用、過期門檻、行程排行、OTLP 的啟用／端點／間隔／名稱／標頭（值只能更換不能檢視）與測試連線。設定存在資料庫 `settings` 表、優先於 `kairos.toml`，環境變數鎖定（`409 locked_by_environment`），寫入稽核（`export.change`、`export.otlp_test`）。
- **API**：`GET`／`PUT /api/v1/export/settings`、`POST /api/v1/export/otlp/test`（API.md 第 16 節）；新權限 **`export:write`**（只有管理員；0.4／0.5 的舊範圍 token 沒有）。`GET` 的 `otlp.secrets_available` 表示 `alerts-secrets.key` 可以使用（`false` 時不能保存標頭），主控台據此一開始就停用標頭欄位，不必等保存被 503 拒絕。
- **`/metrics`**：`kairos_export_prometheus_enabled`、`kairos_export_prometheus_scrapes_total`、`kairos_export_otlp_enabled`、`kairos_export_otlp_queue_points`、`kairos_export_otlp_sent_points_total`、`kairos_export_otlp_requests_total{result}`、`kairos_export_otlp_dropped_points_total{reason}`、`kairos_export_otlp_last_success_timestamp_seconds`。
- **依賴**：沒有新增 crate（OTLP 的 protobuf 為手寫編碼，HTTP 用戶端沿用 hyper 與 tokio-rustls）；指標轉出本身不變更資料庫 schema。
- **量測**（`export_bench`，10,000 台、每台 43 個指標）：Prometheus 輸出 359 ms、56.9 MB；保存最新值約 7 KB／台；OTLP 編碼約 83 萬點／秒（EXPORT.md 第 5 節）。

### Kairos：個資保護（S6；`kairos/PRIVACY.md`）

- **`[privacy]`**（Kairos）：`store_peer_ip`（`full`／`truncate`：IPv4 /24、IPv6 /48／`none`）、`hostname`（`keep`／`pseudonymize`：以 `data/privacy.key` 的 HMAC 換成 `host-xxxxxxxxxxxx`，顯示名稱 `name-…`；不可逆）、`process_command`（`keep`／`redact`／`drop`）、`process_user`（`keep`／`pseudonymize`／`drop`）、`inventory_network`（`keep`／`truncate`：位址 /24、/48、MAC 只留廠商碼／`drop`）。在 8443 的連線與 8444 的註冊申請進入時一次轉換，狀態表、`agents.json`、清冊、report、資產、告警、JSONL 鏡像、主控台與匯出都只看到轉換後的值；日誌、稽核日誌與註冊申請的來源 IP 不受影響（PRIVACY.md 第 2.3 節）。要求假名化而金鑰無法建立或讀取時 Kairos 不啟動。
- **刪除主機資料**：主控台「主機詳情 › 更多 › 刪除主機資料」（只有管理員，新權限 `hosts:purge`；主機須離線；影響預覽＋L2 輸入主機名稱）、`GET/POST /api/v1/hosts/{agent_id}/purge`（`impact_token`）、`kairos hosts purge <agent_id> [--dry-run]`（實際刪除需停止 Kairos，執行中以 `KRS-E4304` 拒絕）。刪除 report、行程快照、彙總、資產與變更歷史、告警事件（含處理與發送紀錄）、註冊申請、憑證紀錄與清冊；先以 `cessationOfOperation` 撤銷這台主機的有效憑證，已撤銷未過期的憑證紀錄保留到過期後 1 天（CRL 需要）；**稽核日誌不刪除**（雜湊鏈），另記一筆 `host.purge`。執行中由 report 的 writer 逐步刪除（每一步 5,000 列，與 report 交錯），刪除後 10 分鐘內拒絕該主機的寫入，避免被建立回來。
- **個資匯出**（資料主體存取請求）：`GET /api/v1/hosts/{agent_id}/personal-data`（新權限 `hosts:personal_data`，只有管理員）、`kairos hosts export <agent_id> [--out]`、主控台「更多 › 匯出個資（JSON）」，`kairos.host.personal-data/v1`：清冊、資產、行程快照摘要、report 的筆數與範圍、告警事件、註冊申請、憑證紀錄、相關的稽核紀錄；每次記稽核。
- **`kairos hosts pseudonym <名稱>…`**：以金鑰算出假名（「`ann-laptop` 在 Kairos 中叫什麼」）。
- **主機 API 的 `pseudonymized`**：主機名稱是假名（`host-` ＋ 12 個 base32 小寫字元）；主控台 S05 頁首的假名說明改用這個欄位，不再自行比對格式。
- **與指標轉出的串接**：刪除主機資料時一併丟棄 Prometheus exporter 在記憶體中的最新值與 OTLP 佇列中這台主機尚未送出的資料（之後的抓取不再輸出）；轉出的主機名稱、顯示名稱與行程排行都是 `[privacy]` 轉換後的值（EXPORT.md 第 2.7 節、PRIVACY.md 第 1 節 #16）。已被 Prometheus 抓取或已推送到 collector 的資料需在該系統處理。

### Pulse 的多個伺服器位址與協定 feature `cluster`（HA L4；`kairos/HA.md` 第 8 節、PROTOCOL.md「叢集與排空」）

- **Pulse `server.fallback_addresses`**：主位址連不上時依序改試的其他 Kairos 節點（"主機" 或 "主機:埠"，IPv6 加方括號，最多 16 個；環境變數 `PULSE__SERVER__FALLBACK_ADDRESSES` 以逗號分隔）。新的鍵、`address` 維持單值，降版安全；安裝字串不變。一輪從主位址開始依序嘗試，收到 `hello_ack` 之前失敗就立即改試下一個（`PLS-W2301`，不增加退避），整輪都失敗才依 `[retry]` 退避（`PLS-E2302`，列出每個位址最後的錯誤）；連上後不主動切回主位址。名稱解析出多個 IP 時依序嘗試，每個最多 3 秒（整個位址仍是 5 秒）。所有位址都以同一個 `server.domain` 驗證伺服器憑證。一次性連線（`pulse enroll` 的確認、輪替後的確認、除役通知、過期憑證的報到）從最近一次連上的位址開始依序嘗試。
- **feature `cluster`**（PROTOCOL.md「叢集與排空」）：Pulse 0.9 一律宣告；Kairos 只在 client 也宣告時才在 `hello_ack` 列出並帶 `server_node`（新設定 `[cluster] node_id`，預設為主機名稱的第一段）。`cluster_id` 留給叢集模式，單機的 Kairos 不送。新增排空訊息 `drain`（Kairos → Pulse）：Kairos 0.9 在**優雅關機**時先對協商了 `cluster` 的連線送 `drain`（`reconnect_within_ms` = `max(10000, 線上連線數 × 5)`）再 `close_notify`，記 `KRS-I8507`；Pulse 收到後停送新的 report、等在途的回應（最多 8 秒）後關閉，在隨機延遲後改連下一個候選位址，不算連線失敗（`PLS-I8507`）。
- **狀態與可觀測性**：`status.json`／`pulse status --json` 的 `connection.server` 改為實際連上的位址，另有 `server_node`、`candidates`、`candidate_index`、`candidate_errors`（只新增欄位，schema 不變）；`pulse status` 顯示「已註冊到 <位址>（Kairos 節點 <名稱>；第 N 個位址，共 M 個）」。本機指標新增 `pulse_server_candidates`、`pulse_server_candidate_index`、`pulse_connect_failovers_total`、`pulse_drains_total`。`pulse check-config` 列出備用位址，`pulse diag` 新增 `candidates.json`（每個位址的名稱解析與 TCP 連線）。
- **Kairos 離線原因 `server_lost`**：服務這台主機的叢集節點失聯（叢集模式由 leader 標記）。0.9.0 只定義名稱：主機 API 的 `offline_reason` 篩選、`/metrics` 的 `kairos_agent_disconnects_total{reason}`（多一個值為 0 的序列）與 Webhook 的固定代碼；單機的 Kairos 不會產生。接收端應忽略不認得的值。

### Kairos：集中設定與 Pulse 遠端升級（F8 K1／K2；`crates/kairos-proto/REMOTE.md`、API.md 第 17 節）

- **8443 的 feature `config`、`upgrade`**：Kairos 0.9 在 SQLite 儲存、**mTLS** 的連線上，client（Pulse 0.9.0 以上）也宣告時列出（`[remote] config_enabled`／`upgrade_enabled`，預設啟用）。`hello_ack` 與回應心跳的 `ack` 帶 `pending`（`config_rev`、提議中的 `upgrade_id`；心跳不查資料庫）；處理 `config_get` → `config`、`config_status` → `ack`、`upgrade_get` → `upgrade_offer`（不是這台主機目前的提議時 `upgrade_unavailable`）、`upgrade_chunk_get` → `upgrade_chunk`（32 KiB 一段；同時下載的主機超過 `max_concurrent_downloads` 時非 fatal 的 `rate_limited`＋`retry_after_ms`）、`upgrade_status` → `ack`（以 (`upgrade_id`, `state`) 去重；`succeeded` 以 hello 的 `agent_version` 交叉確認，不符為 `KRS-W8402`）。沒有協商時這些 `type` 照舊回 `unknown_type`，與 0.8 相同。
- **集中設定**（K1）：設定檔（白名單 28 個鍵，`KRS-E0502`；每一版保留、可回復）、指派（`global`／`group:`／`tag:`／`host:`，預設優先序 0／100／200／1000、可設到期時間）、依優先序合併、`meta.config_rev` 與「只有期望設定的雜湊改變的主機才更新 `desired_rev`」、相同優先序的衝突在儲存時拒絕（409 `assignment_conflict`）、影響 1,000 台以上的變更需要 `impact_token`；依主機的 `agent_version` 只送該版本認得的鍵。Pulse 的 `config_status` 記錄實際生效的值、本機覆蓋、忽略與錯誤（`KRS-W0501`、`KRS-I0503`、`KRS-W0504`、`KRS-I0505` 的日誌）。
- **Pulse 版本庫**（K2）：`POST /api/v1/releases` 匯入一次發佈的 `SHA256SUMS`、`.sig` 與發佈清單，以 `[remote] allowed_signers` 驗證（`release::verify_bundle`；沒有設定時只檢查簽章本身與雜湊並警告），`PUT …/artifacts/{filename}` 上傳發佈檔（必須列在 `SHA256SUMS` 中且雜湊相符；以 SHA-256 命名存入 `data/releases/`，0700）。失敗為 `KRS-E8413`；使用中的版本不能刪（409 `release_in_use`）。
- **升級推送**（K2）：建立（預覽資格分類：已是目標版本、Pulse 太舊、本機停用、缺少信任錨或輔助程式、不支援的平台、沒有發佈檔、`min_upgrade_from`、降版、沒有回滾檔…）→ 送出（L2「升級 N 台」；降版 L3＋原因）→ 立即開始。最多兩批（預設 canary 1%、至多 10 台、觀察 60 分鐘、任何失敗都暫停、人工繼續；其餘 5%），只對連線中且協商了 `upgrade` 的主機提議、同時最多 `max_in_progress` 台；逾時（`expired`、替換後失聯的 `unknown`；Kairos 這端的關機或 `server_lost` 不算主機卡住）、離線寬限（`skipped_offline`）、失敗比例與回滾失敗、簽章失敗時自動暫停（`KRS-W8414`）；暫停、繼續、繼續下一批、取消、重試、回滾失敗標記為已處理。deb／rpm 的提議另附主機目前版本的回滾檔（版本庫有時）。0.9 只支援 Linux 的 tar／deb／rpm。
- **主控台 API**（API.md 第 17 節）：`/api/v1/config/…`、`/releases/…`、`/rollouts/…`、`/hosts/{agent_id}/config`、`/hosts/{agent_id}/upgrade`；主機清單另有 `install`、`config_state` 欄位與 `config_state`、`remote_upgrade` 篩選；SSE `config.host`、`rollout.progress`。新權限 **`config:read`／`config:write`、`releases:read`／`releases:write`、`rollouts:read`／`rollouts:write`／`rollouts:pause`**（`Permission::ALL` 為 45 個；維運者可修改集中設定與暫停推送，建立與執行推送、版本庫的寫入只限管理員）；0.4／0.5 的舊範圍 token 不會取得。稽核類別 `config`、`release`、`rollout` 與 `host.upgrade.*`（一般的升級結果不逐台寫稽核，REMOTE.md D17）。
- **推送 API 的補強**（發佈前，衝刺 9）：推送的 JSON 與 SSE `rollout.progress` 新增 `pause_cause`（暫停中才有值：`manual`、`failure_ratio`、`rollback_failed`、`signature_failed`），`next_action` 的 `paused` 另帶 `cause`；推送清單每筆新增 `rollback_failed_unacknowledged`、`meta` 新增 `paused` 與 `rollback_failed_unacknowledged`（不受篩選影響）。資格分類（預覽、建立、送出）與 `GET /api/v1/hosts/{agent_id}/upgrade` 對連線中的主機改以 hello 的版本、`install`、`os.arch` 與標籤為準（與發提議時一致），沒有協商遠端管理的連線也不必等清冊約 30 秒的同步。允許無法自動回滾的主機而有這類主機時，送出的 L2 確認字串改為設計包的「升級 N 台（無法自動回滾 M 台）」（只接受這個字串）。系統健康新增 `remote` 項目（API.md 第 5.5 節：回滾失敗未處理為嚴重 `KRS-E8408`、自動暫停為警告 `KRS-W8414`、版本庫有發佈版本但沒有信任錨為警告；細節需要 `rollouts:read`）。
- **告警**：新的系統規則 `sys.rollout_paused`「升級推送已暫停」（`KRS-W8414`）、`sys.upgrade_rollback_failed`「升級回滾失敗」（`KRS-E8408`）、`sys.release_signature_failed`「發佈簽章驗證失敗」（`KRS-E8401`），推送繼續或取消、回滾失敗標記為已處理時恢復。
- **`[remote]`**：`config_enabled`、`upgrade_enabled`、`releases_dir`、`allowed_signers`、`extra_principals`、`max_concurrent_downloads`、`max_release_bytes`、`rollout_retention_days`（`kairos.example.toml`；修改後需重新啟動，`allowed_signers` 的內容在每次匯入時讀取）。
- **資料庫 schema v9**（STORAGE.md 第 18 節）：`config_profiles`、`config_profile_revisions`、`config_assignments`、`host_config`、`releases`、`release_artifacts`、`rollouts`、`rollout_hosts`，`hosts.install_method`／`install_upgrade`／`install_config`。刪除主機資料一併刪除 `host_config`、`rollout_hosts`（`remote_management` 筆數），個資匯出另有 `remote_management`。`kairos db backup` 不含版本庫的發佈檔。
- **依賴**：沒有新增 crate；`Cargo.lock` 不變。

### Pulse：集中設定與遠端升級（F8 P1／P2；`crates/kairos-proto/REMOTE.md` 第 3、4 節、PROTOCOL.md「遠端管理」）

- **hello 的 `install`**（不論是否宣告都送）：安裝方式（`/opt/pulse/.install-method` → dpkg → rpm → `/opt/pulse` 的 tar → `unknown`；Windows 為 `msi`／`zip`）、遠端升級的本機狀態（`ready`／`disabled`／`no_trust_anchor`／`no_helper`／`unsupported`）、集中設定的本機狀態。Pulse 0.9.0 宣告 `config`（`[remote_config] enabled`，停用後第一次連線也宣告一次以送出 `state: disabled`）與 `upgrade`（`install.upgrade` 為 `ready` 時）；`release-meta.toml` 的 `remote_features` 改為 `["config", "upgrade"]`。
- **集中設定**：`pending.config_rev` 與已套用的版本不同時 `config_get`，以 `kairos_proto::mgmt::settings` 的 `check_remote`／`merge_key` 合併（內建預設 < 下發 < `pulse.toml` < 環境變數；`collect.interface_exclude` 取聯集；隱私敏感的鍵只接受更保守的值），合併後以 `AppConfig::validate` 檢查，不合法整份拒絕（`PLS-W0501`）。即時的鍵就地生效（`log.level`、取樣間隔與採集範圍、資產、緩衝上限；`retry.*` 與補送間隔在下一次連線），需要重新連線的鍵在送出 `config_status` 後重建取樣、本機緩衝與連線。快取 `<狀態目錄>/remote/config.json`（0750／0640、寫暫存檔再改名、雜湊一致性；損毀或無法寫入時 `PLS-E0506`），Kairos 停機或重新啟動後沿用；確認期與 last-known-good 回復（`PLS-W0504`）、崩潰保護；連上不支援 `config` 的 Kairos 時快取保留不用。`config_status` 在每條新連線的 `hello_ack` 之後與每次處理 `config` 之後送出（`effective` 為白名單的每個鍵，`overridden`／`ignored`／`errors` 依 PROTOCOL.md）。
- **遠端升級（Linux tar／deb／rpm）**：`pending.upgrade_id` → `upgrade_get` → 提議的本機檢查（停用、外部 URL、平台與格式、`staging_space_needed`）→ 本機信任錨驗證（`verify_bundle`、`check_artifact`）與版本政策（`check_version_policy`）→ 經 8443 分段下載（2 段在途、斷線與重新啟動後續傳、`rate_limited` 依 `retry_after_ms` 等待、3 次雜湊不符放棄）→ 暫存 → 進入 `apply_window` 時先向 Kairos 再確認提議（推送已暫停或取消時不替換）→ 送 `applying`、寫 `request.json`。`upgrade_status` 先放進 `<狀態目錄>/upgrade/state.json` 的 outbox，收到 `ack` 才移除（at-least-once）；替換後的 Pulse 讀 `result.json` 回報 `succeeded`／`failed`／`rolled_back`／`rollback_failed`。
- **特權輔助程式** `pulse upgrade apply`（`pulse-upgrade.path`／`pulse-upgrade.service`，root、不連網路）：複製暫存（`O_NOFOLLOW`、`st_nlink == 1`、大小上限）後只對複本操作，以 root 擁有的 `pulse.toml` 重新驗證，預檢新版（以 `pulse` 帳號執行 `--version` 與 `check-config --json --with-remote`），保留 `pulse.prev` 後替換（tar 改名並在 unit 沒被改過時換新；`dpkg -i --force-confold`；`rpm -U`；被鎖住時每 30 秒重試、最多 10 分鐘），`systemctl restart`，健康確認（`effective_health_timeout_secs`、`NRestarts`、狀態檔的 PID 與版本、已註冊且有 report 被確認、穩定 60 秒），失敗自動回滾（`PLS-W8407`；回滾也失敗 `PLS-E8408`，保留所有暫存），重新開機後依 `/var/lib/pulse-upgrade/<id>/state.json` 接續。
- **套件與安裝**：deb／rpm 安裝並啟用 `pulse-upgrade.path`、寫入 `/opt/pulse/.install-method`，移除時停用；tar 發佈包附兩個 unit（INSTALL.md 第 10.1 節）。`pulse-install.sh` 把 `--allowed-signers` 另外放到 `/etc/kairos-pulse/allowed_signers`，新增 `--no-remote-upgrade`、`--no-remote-config`。
- **新設定與命令**：`[remote_config] enabled`；`[upgrade] enabled`、`allowed_signers`、`extra_principals`、`allow_downgrade`、`url_allowlist`、`external_ca`（後兩者 0.10.0 起使用）、`health_timeout_secs`。`pulse config show [--effective] [--sources] [--json]`、`pulse upgrade status [--json]`、`pulse upgrade rollback --yes`、`pulse upgrade apply --request <檔案>`（內部）、`pulse check-config --with-remote`。`pulse status` 的「設定」列附集中設定的狀態，有進行中的升級或最近 24 小時的結果時另有「升級」列（回滾失敗時結束代碼 9）；`status.json` 與 `--json` 新增 `management`（只新增欄位）。日誌事件 `config.*`（6000～6005）、`upgrade.*`（6100～6110）。
- **與規格的差異**：外部 URL（`source: "url"`）在 0.10.0，0.9.0 一律 `declined`（`PLS-W8411`，`url_not_allowed`）；deb／rpm 的回滾檔只在本機保存過它所屬發佈的驗證紀錄時使用（每次驗證通過的發佈都會保存），否則以 `pulse.prev` 回復（套件資料庫仍為新版，`detail` 註明）；`pulse-upgrade.service` 的 `TimeoutStartSec` 為 2.5 小時（健康確認最長 1 小時，回滾後再確認一次）；rpm 的替換與回滾都以 `rpm -U --oldpackage --replacepkgs` 執行（版本政策依實際的執行檔判斷，套件資料庫可能比執行檔新）。詳見 REMOTE.md 第 17 節。
- **系統層實測後的修正**（REMOTE.md 第 12.3、17.6 節）：Pulse 的 rpm 宣告相依 `cpio`（輔助程式解開新版做預檢；Rocky／RHEL 9 的最小安裝沒有），cpio 無法執行時立即失敗（原本卡到 `TimeoutStartSec`）；`pulse upgrade status`／`rollback` 在任何目錄執行都能判斷 tar 安裝；回滾確認期間重新開機後 `detail` 仍附原本的原因；段落請求之間至少隔 60 ms，不再超過 Kairos 每條連線的訊息速率（Kairos 不再為每台主機的下載記 WARN）；輔助程式的日誌記下回滾的方式。

### Kairos：註冊與憑證（衝刺 4 延後的小項）

- **手動簽發憑證的匯入**（PKI.md 第 16 節）：`kairos pki certificates import <檔案或目錄>… [--agent-id]`、`POST /api/v1/pki/certificates/import`（`pki.queue:manage`）、主控台「憑證 › 匯入憑證」。外部 CA（含既有的 RSA 根 CA）或 Kairos 簽發 CA 直接簽發的既有用戶端憑證加入簽發紀錄，檢查不通過以 `KRS-E1305` 列出原因；清冊沒有的主機新增最小的一列。外部 CA 簽發的憑證在 Kairos 撤銷後，8443 在 hello 時拒絕（`KRS-W1105`）。
- **8444 的下載端點** `/agent/<檔名>`（PKI.md 第 17 節）：`enroll.download_dir` 提供 Pulse 的發佈檔案（只有一般檔案、檔名白名單、不跟隨符號連結；內容類型、`nosniff`、`Content-Disposition`；`SHA256SUMS*` 與 `pulse-install.sh` 為 `no-cache`，其他 1 小時；`ETag`／304；串流）。L11 同時下載數（429，`Retry-After: 15`）。主控台的 `downloads` 回報實際狀態，「建立 token」在可用時顯示一行安裝指令。

### Pulse

- **`pulse enroll`**：送出申請之前（`server-info`、`token-check`）收到 429／503 時依 `Retry-After × U(1.0, 1.2) + U(0, 2 秒)` 等候後重試（合計不超過 `--wait`，沒有指定時 600 秒；互動時顯示倒數，Ctrl+C 取消）。0.8 直接結束（結束代碼 6）。
- **`pulse-install.sh --ca-file <根 CA>`**：以 Kairos 的根 CA 驗證下載端點的 HTTPS（完整性仍以發佈公鑰驗證）。
- **修正：Windows 上 `STATE_DIRECTORY` 永遠被忽略**（CI 的 Windows 端對端測試發現）：這個環境變數以 `:` 分隔取第一個路徑（systemd 的格式），`C:\…` 因此被切成 `C` 而不採用，狀態目錄（集中設定的快取 `remote\config.json`、未設定 `[status] file` 時的狀態檔）一律落在 `%ProgramData%\Pulse`，同一台機器上的多個 Pulse 會共用同一份快取。Windows 上改為把整個值視為一個路徑；沒有設定這個環境變數時（MSI／服務的預設）位置不變，Linux 的行為不變。
- **修正：`pulse enroll` 改寫 `pulse.toml` 後服務讀不到設定**（0.8.0 的 deb／rpm 與一行安裝腳本起；systemd 實測發現）：root 改寫套件的 `/opt/pulse/pulse.toml`（`root:pulse 640`）時新檔案變成 `root:root`，一行安裝腳本、Ansible role 裝好的 deb／rpm 主機一直連 127.0.0.1、不用 mTLS。現在 `pulse.toml` 與 `pulse.toml.bak-<時間>` 都沿用原檔的擁有者、群組與權限（備份原本以 umask 的預設權限寫入）。0.8 已安裝的主機請 `chgrp pulse /opt/pulse/pulse.toml` 後重新啟動。

### 主控台（設計包 `decisions/INTEGRATION.md` §31）

- 主機的最後中斷原因新增 `server_lost`「Kairos 節點失聯」（0.9.0 的單機 Kairos 不會產生）；不認得的 `offline_reason` 一律顯示「其他原因」，原始值在技術細節。Webhook 等接收端也應忽略不認得的值。
- SSE `resync` 依原因顯示 Banner（重新啟動、切換節點〔0.10〕、離線太久），主機清單、總覽與告警事件自動重新載入。
- S21 指標轉出卡片、S05 刪除主機資料與假名化說明、S12 下載目錄不完整（只對管理員）、S14 匯入憑證的文案依 §31 定案；`error-codes.js` 依設計包重新產生（Kairos 152 個代碼）。
- **Pulse 管理**（F8 W1；設計包 S24～S26）：側邊導覽新增「Pulse 管理」區塊——**集中設定**（同步狀態、依 `config/schema` 產生的設定檔表單、影響預覽與 1,000 台以上的確認、指派與衝突、版本歷史與回復）、**升級推送**（單頁的建立推送與資格預覽、L2「升級 N 台」／降版 L3 送出、詳情的批次與主機表、暫停／繼續／取消／重試；計數泡泡＝已暫停的推送＋未處理的回滾失敗，取自推送清單的 `meta`；自動暫停的推送標題為「已自動暫停：…」並附 `KRS-W8414`，主控台暫停不附代碼；允許無法自動回滾的主機時 L2 輸入「升級 N 台（無法自動回滾 M 台）」）、**Pulse 版本庫**（匯入時驗證簽章並逐檔上傳、詳情、刪除 L2）。主機清單新增「設定狀態」「遠端升級」篩選；主機詳情新增「設定」分頁與「升級」卡片（回滾失敗的「標記為已處理…」）。功能關閉（503 `remote_unavailable`）時顯示說明；權限依 7 個新權限隱藏操作。雙人覆核、維護時段、外部 URL、Windows 推送在 0.10，主控台不顯示。
- **S20「遠端管理」列**（需要 `rollouts:read`，沒有權限時不顯示）：進行中與已暫停的推送數、版本庫的版本數；回滾失敗未處理、自動暫停、信任錨未設定時為警告或嚴重，附前往升級推送或 Pulse 版本庫的捷徑。
- **S14 匯入手動簽發的憑證**：結果留在對話框內（摘要 `role="status"`，未通過的逐張列出並可展開 `KRS-E1305` 的三段式，全部未通過時為「無法匯入」），可以繼續貼上下一批（設計包 S14「0.9 規格」）。

### 共用 crate：遠端管理的協定與信任鏈（F8 S1；PROTOCOL.md「遠端管理」、`crates/kairos-proto/REMOTE.md`）

- **協定**：REMOTE.md 第 2 節搬入 PROTOCOL.md「遠端管理（feature `config`、`upgrade`）」，範例改為 `kairos-proto` 的測試向量。新訊息 `config_get`／`config`／`config_status`、`upgrade_get`／`upgrade_offer`／`upgrade_chunk_get`／`upgrade_chunk`／`upgrade_status`，hello 的選用物件 `install`（`method`、`upgrade`、`config`），`hello_ack` 與回應心跳的 `ack` 的 `pending`（`config_rev`、`upgrade_id`），新錯誤碼 `upgrade_unavailable`，`rate_limited` 另帶 `retry_after_ms`。兩個 feature 都是雙方宣告才使用：Kairos 0.9.0（K1／K2，見上方「Kairos：集中設定與 Pulse 遠端升級」）與 Pulse 0.9.0（P1／P2，見上方「Pulse：集中設定與遠端升級」）都已實作並宣告；任一端是 0.8 以前時不協商，線上行為與 0.8 相同（Kairos 對這些 `type` 照舊回 `unknown_type`，Pulse 不會送出）。
- **`kairos-proto::mgmt`**：上述訊息的型別與驗證、協商（`MgmtSession`、`ServerMgmtPolicy`、`client_mgmt_features`）、`Pending::for_session`、設定的正規化 JSON 與雜湊（`canonical_json`、`settings_hash`）。**設定白名單** `mgmt::settings`：28 個可下發的鍵的型別、範圍、套用方式（即時／重新連線）、最低 Pulse 版本、隱私規則（`processes.command` 與資產序號、網路位址只能下發更保守的值）與合併方式（`collect.interface_exclude` 取聯集），以及兩端共用的 `validate_profile`（Kairos `KRS-E0502`）、`check_remote`／`merge_key`／`needs_reconnect`（Pulse）、`overlay`／`filter_for_version`（Kairos）。
- **`kairos-proto::release`**：`ssh-keygen -Y sign` 簽章（SSHSIG）的驗證與 `allowed_signers` 的解析（只接受 `ssh-ed25519`、`namespaces`／`valid-after`／`valid-before`，其他選項與 `cert-authority` 整行拒絕），以 ring 的 Ed25519／SHA-512 實作、**不呼叫系統的 `ssh-keygen`**；`SHA256SUMS` 的解析與比對、發佈檔名 `<名稱>-<版本>-<commit>-<os>-<arch>.<格式>`、發佈清單 `pulse-<版本>-<commit>-release.json` 的型別（`min_kairos`、`min_upgrade_from`、`remote_features`、`unit`、`breaking`）、SemVer 比較、REMOTE.md 第 4.2.2 節的驗證流程（`verify_bundle`、`check_artifact`，簽章 8401／其他 8402）與降版政策（`check_version_policy`）。測試向量 `crates/kairos-proto/testdata/release` 是以 OpenSSH 9.6 的 `ssh-keygen -Y sign -n kairos-pulse-release` 產生的真實簽章（與 RELEASING.md 第 3 節相同的參數）。
- **發佈清單**（REMOTE.md R1 的清單部分）：Pulse 的打包另產生 `pulse-<版本>-<commit>-release.json`（`packaging/release-manifest.sh`，手動欄位在 `pulse/packaging/release-meta.toml`，unit 的雜湊取自 git 歷史），列在 `SHA256SUMS` 中、受同一個簽章保護；發佈檔由 20 個變為 21 個。`-dirty` 的建置不產生。`remote_features` 為 `["config", "upgrade"]`（P1／P2 已實作）。
- **依賴**：沒有新增 crate（base64 為 crate 內的實作，Ed25519 與 SHA-512 用既有的 ring）；`Cargo.lock` 不變。
- S1 階段 Kairos 與 Pulse 只做了編譯所需的調整（Kairos 一律回 `unknown_type`、`hello_ack` 不帶 `pending`；Pulse 忽略新的 server 訊息）；兩端的行為已分別由 K1／K2 與 P1／P2 取代。

### 其他變更

- **安全性**：`rustls-webpki` 升級到 0.103.16（RUSTSEC-2026-0382：可能接受含已外洩 emSign 中繼憑證的憑證路徑），Kairos 與 Pulse 都受影響。`rusqlite` 升級到 0.41.1（`libsqlite3-sys` 0.39.1，內含的 SQLite 3.54 以上；RUSTSEC-2026-0459／CVE-2026-119375：資料庫與頁面大小不同的 `-wal` 檔一起開啟時可能讀到未初始化的記憶體，需要能改寫資料目錄的人才能利用），只有 Kairos 受影響；`tokio` 升級到 1.53.3（RUSTSEC-2026-0386：`AsyncFd` 的安全建構函式不符合 I/O safety，Kairos 與 Pulse 都沒有使用 `AsyncFd`）。
- `kairos db backup --include-keys` 一併加密備份 `data/privacy.key`。
- 主控台 API 的 `downloads` 另有 `base_url`、`install_script`、`files`、`product_code`；模式的 `stats` 另有 `downloads_in_progress`。
- 註冊服務每小時另外刪除「清冊中已沒有的主機」過期超過 1 天的憑證紀錄。
- 測試掛鉤 `KAIROS_TEST_GRACE_SECS`（debug 建置）：過期寬限期以秒計；`KAIROS_TEST_REMOTE_START_GRACE_MS`：升級推送在啟動後不提議新主機的時間。
- 內部重構（HA L1a，無行為變更）：Kairos 的儲存層改為領域 store 介面（`Store`／`Tx`），SQL 原封不動搬進 `store/sqlite/`，主控台、註冊、告警、PKI 與所有 CLI 不再直接使用 rusqlite（`kairos/STORAGE.md` 第 17 節）；schema、API 回應、錯誤代碼與 report 的寫入效能不變。

### 規格（尚未實作）

- **`kairos/HA.md`**（審計 A2，高可用）：儲存抽象、PostgreSQL 後端、叢集協調、Pulse 多位址等分層設計與量測計畫；第 21 節的 20 項待決已決定（2026-10-19，採建議）。0.9.0 實作 L1a 儲存抽象與 L4 Pulse 多位址（L4 已包含於本版，見上方），PostgreSQL 後端與叢集協調在 0.10.0。STORAGE.md 的「限制與後續」連到 HA.md。
- **`crates/kairos-proto/REMOTE.md`**（F8，集中設定與 agent 遠端升級）：第 14 節 D1～D18 已決定（2026-10-19，採建議）；0.9.0 實作 MVP（D15）。協定部分（第 2 節）已搬入 PROTOCOL.md，共用型別已實作（見上方「共用 crate：遠端管理的協定與信任鏈」）；Kairos 的行為（K1、K2，實作附註與差異見 REMOTE.md 第 13.1 節）與 Pulse 的行為（P1、P2，見 REMOTE.md 第 17 節）都已實作（見上方）；主控台畫面（W1）也已實作（見上方「主控台」）。

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
| `KRS-E0502` | 設定檔的內容不合法（400 `config_invalid`） |
| `KRS-W0501`、`KRS-I0503`、`KRS-W0504`、`KRS-I0505` | Pulse 回報下發的設定被拒、有鍵被本機覆蓋、套用後已回復、集中設定在主機上停用（鏡像 `PLS-*`；日誌） |
| `KRS-E8401` | Pulse 回報發佈包的簽章驗證失敗（稽核 `host.upgrade.signature_failed`，推送自動暫停） |
| `KRS-W8402` | 回報升級成功但版本不是目標版本（主機標為失敗） |
| `KRS-W8407`、`KRS-E8408` | 主機已自動回滾、回滾失敗需要人工處理（稽核；回滾失敗時推送自動暫停） |
| `KRS-E8413` | 匯入發佈版本失敗（簽章、雜湊、清單、檔案不在 `SHA256SUMS` 中；400 `release_signature_invalid`／`release_invalid`／`artifact_*`） |
| `KRS-W8414` | 推送已自動暫停（失敗比例、回滾失敗、簽章失敗） |
| `KRS-W8415` | 目標版本與目前的 Kairos 不相容（`min_kairos`；400 `incompatible_release`） |
| `KRS-I8416` | 主機已升級完成（日誌） |
| `PLS-W0501`、`PLS-I0503`、`PLS-W0504`、`PLS-I0505`、`PLS-E0506` | 集中設定：整份拒絕、本機覆蓋、確認失敗已回復、本機停用、快取無法使用（`pulse/ERROR-CODES.md` 2.9） |
| `PLS-E8401`～`PLS-W8412`、`PLS-I8416` | 遠端升級：簽章、雜湊與平台、信任錨、下載重試、預檢、替換、已回滾、回滾失敗、本機停用、拒絕降版、不支援、空間不足、升級完成（`pulse/ERROR-CODES.md` 2.9；Kairos 端的鏡像代碼見上方 `KRS-*`） |

新增子類別 23「多伺服器位址與叢集連線」、85「叢集與高可用」（另有 F8 與 0.10 用的 05、44、84）；HA 與 F8 原本都暫定使用子類別 84，§31 把 84 給 F8 遠端升級、叢集改為 85（同尾號），因此排空的代碼由工程暫定的 `KRS-I8407`／`PLS-I8407` 改為 **`KRS-I8507`／`PLS-I8507`**；其餘號碼與工程暫定的相同。Pulse 日誌事件 `conn.failover`、`conn.drain` 的 Windows 事件識別碼為 2005、2007。代碼總數 221 → 287（Kairos 152、Pulse 135，含 0.10 與 F8 的已分配代碼）。

### 已知限制

- OTLP 只支援 HTTP＋protobuf（不做 gRPC、不壓縮）；主機指標名稱沿用 OpenTelemetry 的名稱，不加 `kairos_` 前綴（只有 `kairos_host_*`、`kairos_exporter_*` 與行程排行帶前綴）。
- 假名化不可逆（沒有「顯示原名」）；改變 `[privacy]` 只影響之後收到的資料（PRIVACY.md 第 2.2 節）。
- 刪除主機資料不涵蓋日誌、JSONL 鏡像、備份與已送出的通知；`kairos hosts purge` 需要停止 Kairos。
- 外部 CA 簽發、匯入的憑證撤銷後，已連線中的主機在下一次重新連線時才被拒。
- 遠端升級（Pulse 0.9.0）只支援 Linux；Windows 的 MSI／zip、外部 URL 下載、推送的維護時段與雙人覆核在 0.10.0。0.8 → 0.9 必須手動升級一次。
- 遠端升級的系統層驗證（REMOTE.md 第 12.3 節）已在 systemd 容器中以實際的 Kairos 與 Pulse 執行（Debian 12 deb、Rocky 9 rpm、Ubuntu 24.04 tar：升級、自動回滾、健康確認與回滾確認期間重新開機、簽章錯誤；`tests/systemd/`，不在 CI 必跑的範圍）；**沒有實測**：套件資料庫被鎖住時的重試、tar 安裝替換已修改／未修改的 unit、斷電式的重新開機、aarch64、Debian 11／RHEL 8。
- deb／rpm 從 0.9.0 手動安裝後的**第一次遠端升級沒有可用的回滾檔**（REMOTE.md 第 17.3 節第 1 項）：健康確認失敗時以 `pulse.prev` 換回執行檔，服務恢復正常，但 `dpkg -l`／`rpm -q` 仍顯示失敗的版本（`detail` 為 `binary_restored_package_db_shows_<版本>`），直到下一次成功的遠端升級或手動重新安裝。之後的升級以已驗證的回滾檔經套件管理員回滾。
- 遠端升級在容器中執行的 Pulse 一律 `unsupported`（`/.dockerenv` 或 cgroup；以映像檔管理版本）。
- 刪除主機資料沒有雙人覆核（設計包的選項，之後與其他雙人覆核一起做）。
- 遠端管理（F8 MVP）：Windows 的遠端升級、維護時段（推送的 `schedule`／`windows`）、雙人覆核（`rollouts:approve`、`KRS-E6106`）、外部 URL 的版本庫、自訂批次（多於兩批）在 0.10.0；`kairos releases` 的 CLI、SSE `rollout.host`、升級中主機的離線告警抑制、系統健康的「版本庫的檔案遺失」檢查尚未提供（REMOTE.md 第 13.1 節）。降版推送的 L3 確認不要求重新驗證密碼（設計包 §31.12 #7）。主控台尚未提供：S04 agent 版本欄的「低於建議版本」、全域搜尋的推送與設定檔。

### 測試

- 指標轉出：`export::tests`（假的 collector：semconv 名稱與 resource、標頭、重試與 `Retry-After`、TLS 驗證、佇列上限與丟棄策略、回填不覆蓋最新值、刪除主機時丟棄、主控台端點與權限）、`export::prom::tests`（OTel → Prometheus 名稱、text 0.0.4 與 OpenMetrics 以 `kp_obs::prom::parse` 嚴格解析）、`export::settings::tests`（環境變數 ＞ 主控台 ＞ 設定檔 ＞ 預設、標頭加密）、`config::export::tests`；端對端 `tests/e2e/tests/export.rs`（實際的 Kairos 與 Pulse、Prometheus 抓取、OTLP 推送到假 collector、Pulse 結束後 `kairos_host_up` 為 0）；主控台 `e2e-export.mjs`。
- 單元：`privacy::tests`（位址截斷、假名、命令列遮蔽、行程與資產的轉換、金鑰檔）、`store::purge::tests`、`privacy_cli::tests`、`pki::queue::import_tests`、`enroll::downloads::tests`、Pulse 的 `rate_limits_before_the_request_are_waited_out`。
- 整合：`integration_tests::privacy_policy_is_applied_before_anything_is_stored`、`api::privacy_tests::purge_and_personal_data_over_https`。
- 端對端（`tests/e2e`）：`privacy.rs`（假名與網段、主控台與 CLI 的刪除、個資匯出）、`enroll.rs` 的 `grace_period_is_enforced_and_an_override_extends_it`（ENROLL.md 第 19.3 節 #14）、`manually_issued_certificate_is_imported_and_revocation_takes_effect`、`download_endpoint_serves_the_release_files`。
- 主控台（Playwright＋axe）：`e2e-privacy.mjs`（S05 的權限、仍連線中、匯出、L2 刪除；S12 一行指令；S14 匯入），`e2e-rbac.mjs` 加入新按鈕與端點。
- 發佈前的補強（衝刺 9）：`remote::rollout::tests`（`pause_cause`、`next_action.cause`、「升級 N 台（無法自動回滾 M 台）」）、`remote::tests` 的 `preview_and_submit_use_the_live_hello_instead_of_the_inventory`、`hosts_without_a_rollback_artifact_need_the_longer_l2_phrase`、`pause_cause_and_open_rollback_failures_are_in_the_rollout_json`，`api::remote_tests`（清單的 `meta`、系統健康的 `remote` 項目與權限、停用時為正常），`privacy::tests::host_pseudonyms_are_recognized_by_format`、`api::hosts` 的 `pseudonymized`、`export::tests` 的 `secrets_available`；主控台 `e2e-remote.mjs`（不等清冊同步、S20 遠端管理列、無法自動回滾的字串、自動暫停的 `KRS-W8414`、計數泡泡依 `meta`）、`e2e-export.mjs`（金鑰檔無法使用）、`e2e-privacy.mjs`（假名化主機、S14 結果留在對話框內）、`e2e-admin.mjs`（S20 遠端管理列）。
- 主控台的 Pulse 管理：`e2e-remote.mjs`（版本庫的匯入與簽章錯誤、推送的 L2 送出與暫停／繼續／取消、集中設定的設定檔／指派／版本與回復、主機頁的篩選與「設定」「升級」、稽核員與唯讀的權限、功能關閉）；`e2e-rbac.mjs` 加入「Pulse 管理」的頁面、按鈕與 11 個寫入端點。
- Pulse 多位址與 `cluster`：Pulse 的候選清單、輪替與 `drain` 處理、`status`／`check-config`／`diag` 的顯示，Kairos 的 `hello_ack.server_node`、關機排空與 `[cluster] node_id`，`kairos-proto` 的 `cluster` 型別與測試向量；端對端：兩個 Kairos 的換節點與排空。
- 端對端測試的連接埠（`tests/e2e`）：`free_port()` 原本綁定 0 後立即釋放、等 Kairos 之後再綁定，作業系統分配的臨時埠可能在空檔被同時執行的其他測試的連線搶走（偶發 `Address already in use`）。改為從臨時埠範圍之外挑選、同一個測試行程內不重複，並以綁定但不監聽的 socket 佔住到 Kairos 啟動前一刻；Kairos 仍因連接埠被佔用而結束（結束代碼 11）時，等到可以綁定後以相同設定重新啟動（最多 3 次，`start_kairos`）。另修正 `manually_issued_certificate_is_imported_and_revocation_takes_effect` 在回應之後才檢查日誌的競態（改為等待 `KRS-W1105` 出現）。
- 遠端升級的系統層測試（REMOTE.md 第 12.3 節）：`tests/systemd/run.sh`（`build.sh` 以 HEAD 打包 0.9.0 與只改版本號的測試版本、以臨時金鑰簽章；實際的 Kairos 與三台 systemd 容器：deb、rpm 以 `pulse-install.sh` 安裝，tar 依 INSTALL；情境：第一次遠端升級就失敗、成功升級、經套件管理員回滾、健康確認期間重新開機、回滾確認期間重新開機、Kairos 拒絕匯入與 Pulse 拒絕簽章；說明見 `tests/systemd/README.md`）。新增單元測試：`toml_edit` 沿用擁有者與權限、讀不到的 `pulse.toml`、`rpm_extract` 不會卡住、rpm 的安裝參數、命令列的安裝方式偵測、回滾確認期間重新開機的原因、段落請求的間隔。
- 遠端管理的共用型別（`kairos-proto`）：PROTOCOL.md「範例／遠端管理」的 18 則測試向量（含真實的 `SHA256SUMS`、簽章、清單與分段資料，`upgrade_offer` 的向量以測試的 `allowed_signers` 驗證到底）、`mgmt::tests`（協商、`pending` 的容錯、`install`、各訊息的限制、正規化 JSON、白名單與 REMOTE.md 第 3.1 節一致、合併與隱私規則）、`release::tests`（`ssh-keygen` 產生的正確簽章、錯誤 namespace、錯誤金鑰、`hashalg=sha256`、被修改的 `SHA256SUMS`、`valid-after`／`valid-before`、`cert-authority` 與 RSA 等被拒絕的行、客戶重新簽署的 principal、`SHA256SUMS`、檔名、清單、SemVer 先後順序、降版政策）。
- 遠端管理（Kairos K1／K2）：`remote::desired::tests`（選擇器、依優先序合併與來源、到期、相同優先序的衝突、設定檔的檢查與正規化、影響與 `impact_token`、設定狀態的分類、白名單）、`remote::releases::tests`（`ssh-keygen` 的測試向量、另一把金鑰、錯誤 namespace、被修改的 `SHA256SUMS` 與清單、上傳的雜湊與改名、分段讀取）、`remote::rollout::tests`（推送內容的檢查、資格分類、分批與 canary、推送引擎：提議上限與啟動後 1 分鐘、逾時、`server_lost`、離線寬限、失敗比例、觀察期與人工繼續、完成）、`remote::tests`（暫存資料庫與假的時鐘：只有雜湊改變的主機更新 `desired_rev`、`config_status` 不倒退、影響 1,000 台的 `impact_token`、到期指派、hello 的標籤、版本庫的驗證與使用中、推送的提議／下載／回報／自動暫停／重試／取消、回滾失敗與標記、下載名額、SSE 事件；測試用的簽章以 ring 產生 SSHSIG）、`integration_tests` 的 `mgmt_features_need_mtls_sqlite_and_the_client_declaring_them`、`central_config_round_trip_over_mtls`、`remote_upgrade_offer_download_and_status_over_mtls`（真實的 TLS 連線，提議以 Pulse 端的 `verify_bundle` 驗證到底）、`api::remote_tests`（端點、權限、L2 確認、主機清單的篩選）、`auth_tests::every_role_is_denied_what_it_lacks` 加入 F8 的端點、`rbac::tests::remote_management_permissions_follow_the_design_matrix`；端對端 `tests/e2e/tests/remote.rs`（實際的 Kairos 執行檔、測試向量的發佈，以假 client 模擬 Pulse 0.9.0 走完集中設定與升級推送；測試掛鉤 `KAIROS_TEST_REMOTE_START_GRACE_MS`）。
- 集中設定與遠端升級（Pulse P1／P2）：`remote::config::tests`（合併順序、聯集與隱私規則、只能在本機改的鍵、整份拒絕與鍵之間的矛盾、快取的寫入與重新啟動、雜湊不符與快取竄改、確認期與 3 次失敗回復、崩潰保護、重新啟動後接續確認、本機停用只通知一次、舊版 Kairos 不使用快取、`config_rev = 0`、快取無法寫入）、`remote::install::tests`（安裝方式偵測的順序、遠端升級就緒的每個條件、信任錨的擁有者／權限／符號連結／大小）、`remote::upgrade::tests`（以 `kairos-proto/testdata/release` 的真實簽章模擬提議：下載、驗證、暫存、交付前再確認、交付、重新啟動後回報；本機政策的每種拒絕；簽章、清單、雜湊與平台錯誤；3 次雜湊不符；`rate_limited`、續傳與取消；時段與確認期；同一版的新提議沿用暫存；推送取消時不從暫存替換）、`remote::upgrade::helper::tests`（以臨時金鑰簽署的發佈、假的 systemd 與套件管理員：tar 替換與 unit 判斷、崩潰／逾時／結束代碼 3 回滾、回滾失敗、預檢失敗、重新驗證拒絕換掉的檔案與連結、降版與停用、無效的請求、deb 鎖住重試與兩種回滾、rpm、重新開機後接續、unit 檔指向的路徑）、`network::tests::mgmt`（記憶體內的假 Kairos：hello 的 `install` 與 features、集中設定與升級的完整流程、需要重新連線的鍵、舊版 Kairos、durable 連線上回應順序的對應），每一則送出的訊息都以 `kairos-proto` 的 Kairos 端驗證檢查；端對端 `tests/e2e/tests/remote_config.rs`（實際的 pulse 執行檔與測試內的假 Kairos：即時套用、快取、恢復後不重複取得、舊版 Kairos）。
- 集中設定的兩端互通（K1 × P1）：端對端 `tests/e2e/tests/remote_interop.rs`（實際的 Kairos 與實際的 Pulse，SQLite、mTLS）：hello 協商 `config` 並記錄 `install`；以主控台 API 建立設定檔並指派給主機 → Pulse 取得、即時套用（`report_interval_ms`，連線不中斷）並回報，本機 `pulse.toml` 的鍵列為 `overridden`（Kairos 顯示 `overridden`）；修改設定檔後套用新版、`pulse config show --effective` 讀快取；刪除指派後回到第 0 版與內建預設。
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
