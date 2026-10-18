# Kairos 的個資保護（商業審計 S6）

本文件說明 Kairos 與 Pulse 處理哪些個人資料、存在哪裡、保存多久，以及 0.9.0 起可以調整的保存方式（`[privacy]`）、刪除一台主機的所有資料（`kairos hosts purge`、主控台「刪除主機資料」）與資料主體存取請求（個資匯出）。最後附上資料處理協議（DPA）的範本骨架。

- 狀態：**0.9.0〔草稿〕，已實作**（分支 `sprint8/privacy`）。設計包（`design/`）尚未定案的項目列在第 9 節；新的產品代碼為〔暫定〕（第 8 節）。
- 實作：`kairos/src/privacy.rs`（設定、假名化、遮蔽、個資匯出）、`kairos/src/store/purge.rs`（刪除主機資料）、`kairos/src/api/privacy.rs`（主控台 API）、`kairos/src/privacy_cli.rs`（CLI）、`kairos/web/assets/js/screens/host-privacy.js`（主控台）。
- **本文件不是法律意見。** 「個資」「資料主體」「刪除權」等用語以 GDPR 與我國個人資料保護法（以下簡稱個資法）的一般理解撰寫；實際的適用、法律依據與 DPA 條文請由貴單位的法務或個資保護長審閱。

## 1. 個資清單（盤點）

監控資料本身（CPU、記憶體、磁碟）不是個資，但與「某個人使用的電腦」連結之後就可能是（例如以員工姓名命名的筆電）。下表列出 Kairos 與 Pulse 保存、可能含有個資的資料：

| # | 資料 | 可能的個資 | 位置 | 保存期限 | `[privacy]` | 刪除主機資料 |
|---|---|---|---|---|---|---|
| 1 | 主機名稱、顯示名稱、標籤 | 主機名稱常含人名（`ann-laptop`）；顯示名稱、標籤由管理者設定 | `hosts`、狀態表與 `agents.json`、告警事件的標題與通知、JSONL 鏡像的每一行 | 清冊不自動刪除（主機一直在清冊中） | `privacy.hostname = "pseudonymize"`（第 3 節） | 刪除（清冊、告警、通知紀錄） |
| 2 | 來源位址（`peer`） | 員工電腦的 IP（動態位址可連結到人） | `hosts.peer`、狀態表、`agents.json`、JSONL 鏡像、主控台的連線紀錄（記憶體）、被拒的憑證紀錄 `hosts.cert_rejection` | 同清冊 | `privacy.store_peer_ip`（`truncate`／`none`） | 刪除 |
| 3 | 原始 report、彙總 | 使用模式（開機時間、使用率）；本身不含名稱 | `reports`、`rollup_5m`、`rollup_1h` | 7／31／400 天（`storage.*_retention_days`） | — | 刪除 |
| 4 | 行程排行 | 命令列可能含帳號、密碼、檔名；執行身分是員工帳號 | `process_snapshots` | 同原始 report | `privacy.process_command`（預設遮蔽秘密）、`privacy.process_user` | 刪除 |
| 5 | 主機資產 | 序號（可識別裝置）、MAC、介面位址 | `host_inventory`、`inventory_changes` | 資產直到刪除；變更歷史 400 天、每台 1,000 筆 | `privacy.inventory_network`（位址與 MAC）；序號由 Pulse 的 `inventory.hardware_serials` 控制 | 刪除 |
| 6 | 註冊申請 | 主機名稱、顯示名稱、來源 IP、作業系統 | `enroll_requests` | 結束後 30 天 | 主機名稱與顯示名稱依 `privacy.hostname`；來源 IP 不受影響（第 2.3 節） | 刪除 |
| 7 | 憑證的簽發紀錄 | `agent_id`（假名識別碼，憑證內不含主機名稱） | `pki_certificates` | 不自動刪除 | — | 刪除（已撤銷、尚未過期的保留到過期後 1 天，第 4.3 節） |
| 8 | 告警事件、處理紀錄、通知 | 主機名稱、處理人員的帳號 | `alert_events`、`alert_timeline`、`alert_deliveries`；已送出的 Email／Webhook | 已解決的 400 天（`alerts.retention_days`） | 主機名稱依 `privacy.hostname` | 刪除（已送出到外部的通知無法收回） |
| 9 | 稽核日誌 | 主控台使用者的帳號、來源 IP；操作的對象（主機名稱、`agent_id`） | `audit_events`（雜湊鏈＋HMAC） | 1,095 天（`audit.retention_days`） | 不受影響（第 2.3 節） | **不刪除**（第 4.2 節），另記一筆 `host.purge` |
| 10 | Kairos 的日誌 | 來源 IP、主機名稱、`agent_id`、使用者帳號 | `logs/kairos-*.log`、syslog 外送 | `log.retention_days` | 不受影響（第 2.3 節） | 不刪除，依保留期限 |
| 11 | 診斷包 | 日誌、設定 | 管理者執行 `kairos diag`／`pulse diag` 時產生的檔案 | 管理者自行處理 | `--redact-hosts`／`--redact strict`：IP 與主機名稱換成 `ip-1`、`host-1`（對照表另存，不放進診斷包） | 不涵蓋 |
| 12 | 匯出 | 同 1～5 | `kairos reports export`、主控台的 CSV、個資匯出的檔案 | 管理者自行處理 | 匯出的是保存的值（已依 `[privacy]` 轉換） | 不涵蓋 |
| 13 | 資料庫備份 | 同 1～9 | `kairos db backup` 的檔案、`.keys` | 管理者自行處理 | — | 不涵蓋（第 4.4 節） |
| 14 | 使用者帳號 | 帳號、姓名、Email、登入紀錄 | `users`、`sessions`、`role_assignments` | 帳號刪除時刪除 | — | 不涵蓋（`kairos users delete`） |
| 15 | Pulse 主機上的檔案 | 本機的狀態檔、日誌、緩衝中的 report | `/var/lib/pulse`、`/opt/pulse/logs`、緩衝目錄 | Pulse 的 `log.retention_days`、`buffer.max_age_hours` | — | 不涵蓋（在主機上 `apt purge pulse` 或安裝腳本 `--uninstall --purge`） |
| 16 | 指標轉出（0.9.0，[EXPORT.md](EXPORT.md)） | 同 1、3、4（主機名稱、顯示名稱、標籤、指標、行程排行的名稱） | Prometheus exporter 的輸出、OTLP 推送到 collector 的資料；Kairos 只在記憶體中保留每台主機最新的一筆與 OTLP 佇列，設定存在 `settings` 表（不含主機資料） | 外部系統依其政策；記憶體中的資料在重新啟動時消失 | 轉出的是轉換後的值（第 2.1 節） | 記憶體中的最新值與佇列中尚未送出的資料一併丟棄；已送出到 Prometheus／collector 的資料不涵蓋（第 4.4 節） |

## 2. 保存方式的設定（`[privacy]`）

```toml
[privacy]
store_peer_ip = "full"          # full（ip:port）| truncate（IPv4 /24、IPv6 /48，不含埠）| none（不保存）
hostname = "keep"               # keep | pseudonymize（主機名稱 → host-xxxxxxxxxxxx，顯示名稱 → name-…）
process_command = "redact"      # keep | redact（遮蔽命令列中的秘密；預設）| drop
process_user = "keep"           # keep | pseudonymize（user-…）| drop
inventory_network = "keep"      # keep | truncate（介面位址截到 /24、/48，MAC 只留廠商碼）| drop
```

### 2.1 在哪裡轉換

轉換在資料**進入** Kairos 時一次做完：8443 的連線（hello 的主機名稱與顯示名稱、連線的來源位址、report 的行程排行、`inventory` 的網路介面）與 8444 的註冊申請（主機名稱、顯示名稱）。之後的狀態表、`agents.json`、主機清冊、report、資產、告警、JSONL 鏡像、主控台 API、匯出與指標轉出（Prometheus exporter、OTLP；EXPORT.md 第 2.7 節）看到的都是轉換後的值，不需要各自處理，也不會漏掉新的輸出管道。

- `truncate` 的位址（`10.0.3.0/24`）在主控台與 `kairos hosts list` 原樣顯示；IP 篩選（`ip=` 的 CIDR）與「同網段」的判斷以網段的位址計算。`none` 時顯示「—」。
- `process_command = "redact"` 的規則與診斷包的遮蔽相同（`kp-obs` 的 `redact`：`token=…`、Bearer、API／註冊 token、帶帳號密碼或查詢字串的網址），另外遮蔽 `--password x`、`--password=x`、`-password x`（名稱含 password、secret、token 等的參數）與 `mysql -pSECRET` 的寫法。`-p` 開頭的單一破折號參數一律遮蔽，`-pidfile` 這類參數也會被遮蔽（寧可多遮）。Pulse 預設不送命令列（`processes.command = "executable"`），這個設定只影響管理者在 Pulse 開啟完整命令列的主機。
- 主機資產的摘要（`inventory_digest`）以轉換後的內容計算，位址截斷後介面位址的變化（例如 DHCP 換位址）不再寫入變更歷史。

### 2.2 改變設定的效果

設定只影響**改變之後**收到的資料，不會改寫既有的資料：

| 改變 | 既有資料 |
|---|---|
| 開啟 `hostname = "pseudonymize"` | 清冊的主機名稱在主機下次連線、清冊同步時（30 秒內）換成假名；既有的告警事件、註冊申請、稽核日誌、JSONL 鏡像、日誌與備份仍是原本的名稱，依各自的保留期限刪除。需要立即清除時，對個別主機執行「刪除主機資料」 |
| 開啟 `store_peer_ip = "truncate"`／`"none"` | 清冊的 `peer` 在主機下次連線時改寫；JSONL 鏡像、日誌、備份不改寫 |
| 關閉假名化 | 清冊的主機名稱在下次連線時換回原本的名稱 |

### 2.3 不受 `[privacy]` 影響的資料與理由

| 資料 | 理由 |
|---|---|
| Kairos 的日誌（第 1 節 #10） | 日誌是資安與維運紀錄（連線被拒、憑證被撤銷、限速的來源），需要原本的位址才能處理事故。以 `log.retention_days` 控制保存期限；外送 syslog 時由接收端的政策管理 |
| 稽核日誌的來源 IP（#9） | 稽核日誌是存取紀錄（誰、何時、從哪裡），雜湊鏈不可修改，保留是資安的正當利益（GDPR 第 6 條第 1 項 f 款），以 `audit.retention_days` 控制 |
| 註冊申請的來源 IP（#6） | 防濫用的判斷依據（同一來源大量申請、限定網段、L4～L7 限速與封鎖）；申請結束後 30 天刪除 |
| 記憶體中的連線紀錄 | 主控台的「這次啟動後的上線與離線紀錄」依 `privacy.store_peer_ip` 顯示保存的位址；限速與每 IP 上限在記憶體中使用實際位址，不保存 |

## 3. 主機名稱的假名化：設計與取捨

`privacy.hostname = "pseudonymize"`：假名 ＝ `host-` ＋ base32（HMAC-SHA256（金鑰, `host` ‖ 0x00 ‖ 小寫的主機名稱））的前 12 個字元（60 位元）。顯示名稱為 `name-…`、行程的使用者為 `user-…`（不同的前綴，同一個字串得到不同的假名）。

- **金鑰**：`data/privacy.key`（32 bytes，0600），Kairos 在需要時（設定了 `pseudonymize`）第一次啟動建立；金鑰無法建立或讀取時 Kairos **拒絕啟動**（〔暫定〕`KRS-E4005`）——管理者要求假名化，不能退回保存原值。金鑰不在資料庫中，`kairos db backup --include-keys` 會一併加密備份（STORAGE.md 第 9 節）。
- **一致**：同一個名稱在任何主機、任何時間都得到同一個假名，主控台的搜尋、告警、註冊申請的「主機名稱重複」判斷照常運作。
- **查詢**：`kairos hosts pseudonym <主機名稱>...`（`--kind host|name|user`）以金鑰算出假名，回答「`ann-laptop` 在 Kairos 中叫什麼」。

| 方式 | 說明 | 取捨 |
|---|---|---|
| **HMAC，不可逆（採用）** | 不保存原值；知道金鑰的人可以對「候選名稱」算出假名再比對 | 外洩資料庫（沒有金鑰）時無法還原；有金鑰的人仍可以字典攻擊（主機名稱的組合通常有限），因此金鑰要與資料庫分開保管。無法列出「所有主機的原名」，現場需要以候選名稱查詢 |
| 可逆（加密對照表） | 另存「假名 → 加密的原名」，有權限的人可以還原 | 需要新的資料表（schema 升版）與還原的權限、稽核；原名仍以密文存在資料庫中，刪除權的範圍更大。若使用者需要「主控台按一下顯示原名」，可在之後的版本加入（第 9 節） |
| 決定性加密（SIV） | 假名本身就是密文 | 假名很長（主機名稱長度＋28 bytes 的 base32），主控台難以閱讀 |
| 每次匯出才假名化 | 資料庫存原值，匯出或診斷包時才換 | 無法降低資料庫外洩的風險；診斷包已有這個功能（`--redact-hosts`） |

- **換金鑰**：刪除 `privacy.key` 再啟動會建立新的金鑰，**所有主機的假名都會改變**（下次連線時清冊更新），既有的告警、註冊申請中仍是舊假名。本版不提供金鑰輪替的工具。
- **與診斷包的關係**：診斷包的 `--redact-hosts` 是每一份診斷包各自編號（`host-1`、`ip-1`，對照表不放進診斷包），與這裡的 HMAC 假名無關；兩者可以同時使用（診斷包中出現的假名會再被換成 `host-N`）。

## 4. 刪除主機資料

依 `agent_id` 刪除一台主機在資料庫中的所有資料（GDPR 第 17 條的刪除權、個資法第 11 條的刪除請求，或單純的主機汰換）。

### 4.1 流程

| 方式 | 何時 | 說明 |
|---|---|---|
| 主控台：主機詳情 › 更多 › 刪除主機資料（**只有管理員**，`hosts:purge`） | Kairos 執行中 | 影響預覽（各資料的筆數、會撤銷的憑證、保留的項目）→ L2 輸入主機名稱（＋選填原因）→ 刪除後回到主機清單 |
| `POST /api/v1/hosts/{agent_id}/purge` | Kairos 執行中 | 先 `GET` 取得 `impact_token` 與 `confirm_text`，送出時帶 `confirm`（主機名稱）、`impact_token`、`reason`（API.md 第 15 節） |
| `kairos hosts purge <agent_id> --dry-run` | 任何時候（唯讀） | 列出會刪除的筆數 |
| `kairos hosts purge <agent_id>` | **Kairos 停止時** | 互動時輸入主機名稱確認；非互動 `--yes --confirm <主機名稱>`。Kairos 執行中時以 `KRS-E4304`（結束代碼 11）拒絕 |

執行中的刪除由 Kairos 自己完成：

1. 主機必須**離線**（連線中時 409 `host_online`，〔暫定〕`KRS-E4205`）。請先在主機上停止並移除 Pulse，或撤銷憑證讓它斷線。
2. 從狀態表移除（之後的 `agents.json`、清冊同步、主控台都看不到它），並丟棄指標轉出在記憶體中的資料（Prometheus exporter 的最新值、OTLP 佇列中尚未送出的這台主機的資料；EXPORT.md）。
3. 撤銷這台主機尚未撤銷、尚未過期的憑證（`cessationOfOperation`，立即生效；每張一筆稽核 `cert.revoke`）。執行中的 Kairos 數秒內重新產生 CRL；外部 CA 簽發、匯入的憑證由 Kairos 在 hello 時拒絕（PKI.md 第 16 節）。
4. 告警引擎忘記這台主機的進行中事件（之後不再寫入它們）。
5. report 的 writer 逐步刪除（每一步最多 5,000 列、一個交易，與 report 的寫入交錯，不會長時間擋住其他主機）：原始 report、行程快照、彙總、資產變更歷史；最後一步在同一個交易中刪除資產、序列、去重鍵、告警事件與處理紀錄與發送紀錄、註冊申請、續簽的重放快取、憑證紀錄、主機清冊，並寫入稽核 `host.purge`。
6. 刪除開始後 10 分鐘內，這台主機的 report、資產與清冊同步一律不寫入（避免在途的資料把它建立回來；連線回 `storage_unavailable`）。

中途失敗（磁碟、關機）時已刪除的部分不會復原，清冊仍在；重新執行同一個操作即可接續完成。刪除完成後記一行 `INFO`「資訊 KRS-I4204：已刪除主機 … 的資料」（〔暫定〕代碼）。

CLI 需要停止 Kairos 的原因：執行中的 Kairos 在記憶體中有這台主機的快取、尚未寫出的彙總與狀態表，另一個行程直接刪除時，Kairos 之後的寫入會把資料（甚至清冊）寫回去。與 `kairos reports import` 相同，以 SQLite 的獨占鎖確保 Kairos 沒有在執行。

### 4.2 稽核日誌不刪除（雜湊鏈與刪除權的取捨）

稽核日誌（`audit_events`）的每一筆都包含前一筆的雜湊並帶 HMAC（API.md 第 12.8 節）：刪除或修改其中任何一筆，`kairos audit verify` 與每日的自我驗證都會失敗（`KRS-E6201`），稽核日誌就失去證明力。因此「刪除主機資料」**不刪除**稽核紀錄，而是：

- 另記一筆 `host.purge`（`category: host`）：執行者、來源 IP、`agent_id`、原因、撤銷的序號、各資料表刪除的筆數；**不含主機名稱與位址**（不把要刪除的個資再寫進不可修改的日誌）。
- 既有的稽核紀錄可能含有這台主機的名稱（例如當初核准註冊申請的紀錄）。它們依 `audit.retention_days`（預設 1,095 天）到期刪除；刪除時保留雜湊鏈的錨點，鏈仍可驗證。
- 法律上的依據（請法務確認）：GDPR 第 17 條第 3 項 b 款（履行法定義務）與 e 款（法律主張的建立、行使或防禦），以及第 6 條第 1 項 f 款的正當利益（資安稽核）；稽核紀錄的內容限於「誰在何時對哪個對象做了什麼」。若貴單位的政策要求更短的保存期限，可調整 `audit.retention_days`（下限 90 天）。

### 4.3 為了撤銷清單保留的憑證紀錄

已撤銷、尚未過期的憑證若從簽發紀錄刪除，下一版 CRL 就不會再列出它，等於**取消撤銷**。因此刪除時保留這些紀錄（只有序號、`agent_id`、效期與撤銷原因；憑證本身不含主機名稱），在憑證過期 1 天後由註冊服務每小時的清除刪除。尚未撤銷的有效憑證一律先撤銷再保留，不會留下有效又無法撤銷的憑證。

### 4.4 不涵蓋的資料

| 資料 | 處理方式 |
|---|---|
| Kairos 的日誌、syslog 接收端 | 依 `log.retention_days`／接收端的政策；需要立即清除時人工處理 |
| JSONL 鏡像（`storage.jsonl_mirror = true`）的 `reports-*.jsonl` | 依 `storage.retention_days`；檔案是唯讀的歷史紀錄，Kairos 不改寫 |
| 資料庫備份（`kairos db backup`） | 依備份的保存政策；還原舊的備份會把已刪除的主機帶回來，還原後請再執行一次刪除 |
| 已送出的通知（Email、Webhook、LINE、Teams） | 在接收端處理 |
| 已轉出的指標（Prometheus 已抓取的序列、已推送到 OTLP collector 的資料） | 在 Prometheus／collector 端處理（依其保存期限，或以其刪除 API，例如 Prometheus 的 `delete_series`）；Kairos 停止輸出這台主機 |
| 匯出、診斷包、個資匯出的檔案 | 由取得的人處理 |
| Pulse 主機上的資料 | 在主機上完全移除 Pulse（第 1 節 #15） |

刪除之後，同一台主機若再次以有效的身分連線（例如沒有撤銷的外部 CA 憑證、或未啟用 mTLS 的測試環境），會從頭建立新的清冊資料。請先在主機上移除 Pulse。

## 5. 資料主體存取請求（個資匯出）

`GET /api/v1/hosts/{agent_id}/personal-data`（只有管理員，`hosts:personal_data`）、`kairos hosts export <agent_id> [--out <檔案>]`（Kairos 執行中也可以）與主控台的「更多 › 匯出個資（JSON）」輸出同一份 JSON（`schema`：`kairos.host.personal-data/v1`）：

| 欄位 | 內容 |
|---|---|
| `host` | 清冊的一列（主機名稱、顯示名稱、標籤、作業系統、來源位址、首次與最後出現、生命週期、最後一筆 report） |
| `inventory`、`inventory_changes` | 目前的資產與變更歷史 |
| `process_snapshots` | 行程快照的筆數、時間範圍與最新一筆 |
| `reports` | 原始 report 的筆數與時間範圍、彙總的列數；指標的數值以 `kairos reports export --agent <agent_id>` 取得（數量可能很大，不放進這份匯出） |
| `alerts` | 這台主機的告警事件與處理紀錄 |
| `enroll_requests` | 註冊申請（不含 CSR 與輪詢憑據的雜湊） |
| `certificates` | 憑證的簽發紀錄（不含憑證的 DER） |
| `audit_events` | 對象是這台主機、它的申請或憑證，或執行者是這台主機的稽核紀錄 |
| `privacy` | 目前的 `[privacy]` 設定（讓收到的人知道名稱與位址是否已假名化） |

- 時間為 RFC 3339（UTC）；資料庫中的二進位欄位（雜湊、DER、壓縮的 report）不輸出。
- 指標轉出（EXPORT.md）不在資料庫保存主機的資料（只有 `settings` 表中的轉出設定），因此個資匯出沒有對應的欄位。
- 每次匯出記一筆稽核 `host.personal_data.export`。匯出的檔案含個資，請以安全的管道交給請求人並刪除副本。
- 請求人是「使用這台電腦的人」而不是主機本身時，請由貴單位確認身分與權限後再提供（Kairos 不判斷）。

## 6. 保存期限的建議

| 設定 | 預設 | 建議 |
|---|---|---|
| `storage.raw_retention_days` | 7 | 依故障調查的需要；原始資料是最細的使用紀錄 |
| `storage.rollup_5m_retention_days`／`rollup_1h_retention_days` | 31／400 | 容量規劃用；只有彙總值 |
| `alerts.retention_days` | 400 | 已解決的事件 |
| `audit.retention_days` | 1,095 | 依資安稽核的法規（第 4.2 節） |
| `log.retention_days` | 見 README | 日誌含原本的位址（第 2.3 節） |
| 主機清冊 | 不自動刪除 | 汰換、離職、借用結束的主機以「刪除主機資料」刪除 |

## 7. 資料處理協議（DPA）範本骨架

> **非法律意見。** 以下只是條文的骨架與填寫提示，協助貴單位與法務起草；條文內容、適用的法規（GDPR、個資法、其他）與責任分配都必須由法務審閱後定稿。〔 〕為待填寫的內容。

1. **當事人與角色**：資料控制者〔委託單位〕；資料處理者〔維運廠商／Kairos 的維運單位〕；再處理者〔無／列出〕。Kairos 為自行架設的軟體，原廠不接觸監控資料〔若提供遠端支援，另列支援時可能接觸的範圍，例如診斷包〕。
2. **處理的目的**：IT 資產與服務可用性的監控、告警與容量規劃〔不得用於員工績效考核或行為監控，若有例外另列〕。
3. **資料主體與資料類別**：第 1 節的個資清單〔刪除不適用的列〕；不處理特種個資。
4. **處理的方式與保存期限**：第 2 節的 `[privacy]` 設定值〔填入實際值〕；第 6 節的保存期限〔填入實際值〕。
5. **安全措施**：mTLS（Pulse 與 Kairos 之間）、主控台的角色與權限、稽核日誌（雜湊鏈＋HMAC）、資料庫與金鑰檔的權限（0640／0600）、備份的加密〔填入貴單位的實體與網路安全措施〕。
6. **資料主體的權利**：存取（第 5 節的個資匯出）、刪除（第 4 節）、更正（主機名稱以 Pulse 的設定更正）〔回覆期限：例如 30 日內〕〔提出請求的窗口〕。
7. **事故通報**：發現個資外洩時〔多少小時內〕通知控制者；內容〔外洩的範圍、可能的影響、已採取的措施〕。
8. **再處理者**：〔是否允許、事前同意的方式〕。
9. **跨境傳輸**：〔Kairos 的部署位置；syslog、通知管道（Email、Webhook、LINE、Teams）的接收端所在地〕。
10. **契約結束**：〔刪除或返還〕資料庫與備份；以「刪除主機資料」或整個資料目錄的刪除完成，並〔出具刪除證明〕；稽核日誌依第 4.2 節的保存期限處理〔或約定提前刪除整個資料庫〕。
11. **稽核權**：控制者〔每年〕可要求提供 `kairos audit verify` 的結果、設定（`kairos config show --effective`）與保存期限的證明。

## 8. 產品代碼（〔暫定〕，待設計包分配）

| 代碼 | 等級 | 何時 |
|---|---|---|
| `KRS-I4204` | 資訊 | 已刪除主機資料（日誌、稽核 `host.purge` 的 `code`） |
| `KRS-E4205` | 錯誤 | 無法刪除主機資料：主機仍連線中（409 `host_online`）、刪除沒有完成（503）、找不到資料（CLI） |
| `KRS-E4005` | 錯誤 | 要求假名化（`pseudonymize`）但 `data/privacy.key` 無法建立或讀取，Kairos 不啟動 |

既有代碼：Kairos 執行中時 `kairos hosts purge` 以 `KRS-E4304`（資料庫正在使用中）拒絕。號碼依設計包 `copywriting.md` §3.2 的子類別（40 資料目錄、42 保留與清除）選用尚未被 Kairos 與 Pulse 佔用的號碼。

## 9. 待決定事項

| # | 事項 | 選項 | 建議 |
|---|---|---|---|
| 1 | 假名化是否需要可逆 | (a) 維持不可逆（目前）；(b) 加入加密對照表與「顯示原名」的權限（schema 升版、稽核每次還原） | (a)：先以 `kairos hosts pseudonym` 滿足現場的查詢；有客戶明確需要時再做 (b) |
| 2 | `process_command` 的預設 | (a) `redact`（目前，0.9 起）；(b) `keep`（與 0.8 相同） | (a)：只影響 Pulse 開啟完整命令列的主機，遮蔽秘密沒有壞處 |
| 3 | 刪除主機資料是否要雙人覆核 | 設計包 `ia.md` 列為「可選的雙人覆核」 | 與其他雙人覆核的操作一起做（目前沒有雙人覆核的機制） |
| 4 | 日誌是否也依 `store_peer_ip` 遮蔽 | (a) 不遮蔽（目前，日誌是資安紀錄）；(b) 另加 `privacy.log_peer_ip` | (a)，以 `log.retention_days` 控制；有客戶要求時再做 (b) |
| 5 | 設計包 | S05「刪除主機資料」的影響預覽與文案、S12 一行指令的 `kairos-ca.pem` 說明、S14「匯入憑證」對話框、`hosts:purge`／`hosts:personal_data` 權限名稱與第 8 節的代碼 | 請設計包負責人定案（實作的文案在 `kairos/web/assets/js/core/strings-privacy.js`） |
