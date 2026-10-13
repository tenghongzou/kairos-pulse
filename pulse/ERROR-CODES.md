# Pulse 錯誤代碼與日誌事件對照

本文件列出 Pulse 0.4〔草稿〕已對應到設計包代碼的地方（代碼 → 程式位置 → 觸發條件 → 在哪裡看得到），以及尚未實作的代碼。代碼的唯一來源是設計包 `design-system/copywriting.md` §3.3（Pulse 共 109 個）；日誌事件依 `pulse/log-ux.md` §4。發佈包內附本文件，`pulse explain <代碼>` 印出代碼的標題並指向這裡。

## 1. 代碼出現在哪裡

| 管道 | 格式 | 例子 |
|---|---|---|
| 日誌（`logs/pulse-YYYY-MM-DD.log`、終端） | 訊息開頭為「`<event> <code> `」，沒有代碼時為 `-`（`log-ux.md` §6.1 的欄位順序） | `2026-10-13 14:22:16 [WARN] buffer.full PLS-W4202 本機緩衝已滿（待送 50000 筆…）` |
| stderr／journal（需要人工處理的錯誤） | 與日誌的 `ERROR` 行相同，含事件與代碼 | `pulse[1234]: [ERROR] auth.rejected PLS-E1105 連線中斷（連續第 1 次失敗）：…` |
| `pulse status` | 檢查列的括號內、三段式說明的第一行；`--json` 的 `data.checks[].code`、`data.issues[].code` | `○ 離線 連線   重新連線中：…拒絕連線（PLS-E2003）` |
| `pulse status --brief` | 空白分隔的欄位 | `offline   plc-hmi-07 reconnecting 12m30s PLS-E2003 backlog=149` |
| 啟動失敗（日誌初始化之前） | stderr：`錯誤 <代碼>：…` | `錯誤 PLS-E0101：無法載入設定：…` |
| 命令列用法錯誤 | stderr：`錯誤：<代碼>：…` | `錯誤：PLS-E0001：pulse status 未知的參數「--bogus」…` |

日誌中的事件與代碼固定不翻譯，規則與搜尋請比對這兩欄（例如 `grep ' buffer.full PLS-W4202 '`），不要比對訊息文字。

## 2. 已對應的代碼（56 個）

程式位置為 `pulse/src/` 下的檔案與函式。「狀態」指 `pulse status` 的檢查列（服務、連線、回報、身分、設定、時鐘）。

### 2.1 命令列、設定

| 代碼 | 程式位置 | 觸發條件 | 看得到的地方 |
|---|---|---|---|
| `PLS-E0001` | `cli.rs` `parse_global`、`parse_status`；`status_cmd::explain` | 未知的參數或命令；`pulse explain` 不認得的代碼 | stderr，結束代碼 2 |
| `PLS-E0002` | `cli.rs` `parse_status`；`status_cmd::once` | 參數值不合法（`--watch` 秒數、`--exit-codes`、`--color`、缺值）、`--all`（尚未支援）、`--config` 指定的檔案不存在 | stderr，結束代碼 2 |
| `PLS-E0003` | `cli.rs` `parse_status` | `--brief` 與 `--json`、`--watch` 與 `--json` 同時使用 | stderr，結束代碼 2 |
| `PLS-E0101` | `main.rs` `main`；`status_cmd::config_error_code` | 設定檔語法或型別錯誤（啟動時結束代碼 3）；`pulse status` 讀到的設定檔有誤 | 啟動：stderr；狀態：設定列 ✗（例如「第 4 行語法錯誤」） |
| `PLS-E0201` | 同上 | 設定值超出範圍、設定互相矛盾等驗證失敗（`AppConfig::validate`） | 同上 |
| `PLS-W0302` | `tls.rs` `build`（日誌）；`main.rs` `config_warnings`（狀態） | `accept_invalid_certs = true` | `config.warning`（ERROR 等級，與先前相同）；設定列 ⚠ |
| `PLS-W0303` | `main.rs` `config_warnings` | 設定了 `server.ca_cert` 又讓 `use_system_roots = true` | 設定列 ⚠ |
| `PLS-W8101` | `main.rs` `startup_location_messages` | 工作目錄是 `System32`／`SysWOW64` | `config.warning`；設定列 ⚠ |

### 2.2 TLS、身分

| 代碼 | 程式位置 | 觸發條件 | 看得到的地方 |
|---|---|---|---|
| `PLS-E1101` | `network.rs` `error_code` | 伺服器以 TLS alert `CertificateRequired` 拒絕 | `auth.client_cert_required`；連線列 ✗ |
| `PLS-E1102` | `network.rs` `error_code`；`main.rs` `run` | `auth_failed`；啟動時本機檢查 SAN 與 agent_id 不符（結束代碼 4） | `auth.mismatch`／`agent.fatal`；連線列 ✗ |
| `PLS-E1103` | `network.rs` `error_code` | `auth_failed`：`exactly one DNS SAN` | `auth.mismatch`；連線列 ✗ |
| `PLS-E1105` | `network.rs` `error_code` | `certificate_rejected`、TLS alert `CertificateRevoked`，以及伺服器未細分原因的 alert（`BadCertificate`、`AccessDenied` 等，設定了客戶端憑證時） | `auth.rejected`；連線列 ✗、身分列「已撤銷（序號 …）」；之後只剩一般網路錯誤時仍顯示「上次被拒」 |
| `PLS-E1108` | `network.rs` `error_code`；`tls.rs` `log_findings`；`status_cmd/eval.rs` `identity_row` | TLS alert `CertificateExpired`；本機檢查憑證已過期（啟動時與每 24 小時）；狀態依校正後的時間判斷 | `auth.expired`／`cert.expired`；身分列 ✗（沒有有效時鐘偏差時 ⚠「尚未經 Kairos 確認」） |
| `PLS-E1109` | `network.rs` `error_code` | TLS alert `UnknownCA` | `auth.untrusted`；連線列 ✗ |
| `PLS-W1110` | `tls.rs` `log_findings`；`eval.rs` `identity_row` | 客戶端憑證 30 天內到期（日誌）；14 天內到期（狀態，這個版本不自動更新） | `cert.expiring`；身分列 ⚠、`--brief` 的 `cert=` |
| `PLS-E1203` | `network.rs` `error_code` | `certificate_rejected`：`revocation status unavailable` | `auth.server_crl`；連線列 ✗ |
| `PLS-E1401`～`PLS-E1405` | `network.rs` `error_code`（依 `server_cert_problem`） | 伺服器憑證不受信任／名稱不符／過期或尚未生效／被撤銷／用途不符或被標記為 CA | `tls.server_cert_rejected`；連線列 ✗（「無法連線：伺服器憑證…」） |
| `PLS-E3102` | `network.rs` `error_code` | 連續 2 次以上 `superseded` | `conn.superseded`；連線列 ✗ |
| `PLS-W3101` | `network.rs` `error_code` | 單次 `superseded` | `conn.superseded`（WARN）；連線列 ○ |
| `PLS-E3103` | `main.rs` `run` | `agent.id` 內容不合法（啟動時結束代碼 4） | `agent.fatal` |
| `PLS-E3104` | `main.rs` `run` | `agent.id` 無法建立或讀取（結束代碼 4） | `agent.fatal` |
| `PLS-E8304` | `tls.rs` `log_findings`；`eval.rs` `identity_row` | 客戶端憑證尚未生效（本機時鐘可能落後） | `cert.expiring`；身分列 ⚠ |
| `PLS-W8201` | `main.rs` `run` | `agent.id` 權限過寬 | `config.warning`；身分列 ⚠ |
| `PLS-W8202` | `tls.rs` `build` | 私鑰檔案權限過寬 | `config.warning`；身分列 ⚠ |

### 2.3 連線與協定

| 代碼 | 程式位置 | 觸發條件 | 看得到的地方 |
|---|---|---|---|
| `PLS-E2002` | `network.rs` `connect_error_code` | 名稱解析失敗 | `conn.lost`；連線列 ○（滿 30 分鐘 ✗） |
| `PLS-E2003` | 同上 | 連線被拒絕 | 同上（範例見 `cli-spec.md` §4.5） |
| `PLS-E2004` | 同上；`error_code` | TCP 連線逾時、主機或網路無法到達 | 同上 |
| `PLS-E2005` | `network.rs` `error_code` | TLS 握手逾時、送出 hello 後沒有 `hello_ack`、讀取閒置逾時 | 同上 |
| `PLS-E2101` | `network.rs` `error_code` | `unsupported_version`、`hello_ack` 的 proto 不在支援範圍 | `proto.incompatible`；連線列 ✗ |
| `PLS-E2102`～`PLS-E2104` | `network.rs` `error_code` | 註冊前的 `hello_required`、`too_long`（或伺服器送來的單行超過上限）、`bad_request` | `conn.lost`（ERROR）；連線列 ✗ |
| `PLS-E2201` | `network.rs` `run_with` | TLS 握手完成前、伺服器也還沒要求客戶端憑證就被關閉（多半是每 IP 連線上限，審計 A7） | `conn.lost`；連線列 ○ |
| `PLS-E3201` | `network.rs` `error_code` | `bad_request`：`server agent registry is full` | `server.full`；連線列 ✗ |
| `PLS-W2105` | `network.rs` `handle_server_line_in`、`Delivery::on_reply` | `invalid_report`（at-most-once 的日誌；durable 時丟棄並計數） | `report.rejected`；回報列 ⚠「被 Kairos 拒收」 |
| `PLS-W2107` | `network.rs` `log_unparsable`、`handle_server_line_in`、`durable_reply` | 無法解析的行、註冊後重複的 `hello_ack`、其他非致命 `error` | `server.unexpected` |
| `PLS-W2203` | `network.rs` `storage_code` | `storage_unavailable`：`report rate limit exceeded` | `report.rejected`；回報列 ⚠（退避中） |
| `PLS-W4101` | `network.rs` `storage_code` | `storage_unavailable`：無法保存（durable 時保留並退避重送） | `report.rejected`；回報列 ⚠「Kairos 暫時無法保存（PLS-W4101）…秒後重送」 |

### 2.4 本機緩衝、狀態檔、日誌（`decisions/INTEGRATION.md` §25.5）

| 代碼 | 程式位置 | 觸發條件 | 看得到的地方 |
|---|---|---|---|
| `PLS-E4004` | `main.rs` `run` | `buffer.dir` 無法建立或寫入：本次執行改為 at-most-once | `buffer.unavailable`（ERROR，同時寫 stderr）；systemd `Status:`「已註冊：…；無法開啟本機緩衝（PLS-E4004，…）」；回報列 ⚠ |
| `PLS-W4103` | `spool.rs` `ingest` | 無法寫入一筆取樣（該筆遺失）；定期 fsync 失敗 | `buffer.write_failed`（節流 5 分鐘）；最近 1 小時內有寫入失敗時回報列 ⚠ |
| `PLS-W4202` | `spool.rs` `Spool::open`、`ingest` | 待補送資料超過 `buffer.max_mb`／`max_records`，丟掉最舊的 | `buffer.full`（節流 5 分鐘）；最近 24 小時內丟棄時回報列 ⚠（丟棄筆數與取樣時間範圍） |
| `PLS-W4203` | `network.rs` `Delivery::prepare_next` | 待補送資料超過 `buffer.max_age_hours`，送出前丟掉 | `buffer.expired`（節流 5 分鐘）；最近 24 小時內回報列 ⚠ |
| `PLS-W4201` | `log_files.rs` `log_event` | 系統日期異常，暫停清除日誌 | `log.retention_paused` |
| `PLS-E4002` | `main.rs` `main` | 無法初始化日誌（結束代碼 1） | stderr |
| `PLS-E8204` | `local_status.rs` `Tracker::write_locked`、`run_writer` | 預設位置（`/var/lib/pulse`、`%ProgramData%\Pulse`）無法寫入而改寫 `logs/status.json`（記一次）；執行中無法寫入（第一次 `WARN`、連續失敗滿 60 秒 `ERROR` 並寫 stderr、之後每 10 分鐘一則 `ERROR` 摘要、其間 `DEBUG`；恢復時記 `status.write_restored`） | `status.write_failed`；前者為「設定」列 ⚠（提示更新 unit），後者 `pulse status` 隨後顯示「未知」 |
| `PLS-W8005` | `system_info.rs` `Sampler::refresh_and_read`、`check_stuck` | 系統資訊取樣逾時 | `sample.timeout` |

### 2.5 `pulse status` 專用

| 代碼 | 程式位置 | 觸發條件 | 看得到的地方 |
|---|---|---|---|
| `PLS-E8002` | `status_cmd/eval.rs` `evaluate` | 找不到服務（systemd unit／Windows 服務），也沒有狀態檔 | 服務列 ✗，結束代碼 9 |
| `PLS-E8003` | 同上 | 服務管理員回報未執行（failed、inactive、正在重新啟動、已停止），或沒有服務管理員時狀態檔記錄的 PID 已不在 | 服務列 ✗（上次結束時間與結束代碼），其餘列為「最後已知狀態」 |
| `PLS-W8004` | 同上 | 服務在執行，但狀態檔超過 60 秒未更新（單調時鐘）、不存在或無法解析 | 服務列 ⚠，本機狀態「未知」，結束代碼 9 |
| `PLS-E8203` | `status_cmd::permission_denied` | 沒有權限讀取狀態檔 | stderr（`--json` 為 `error`），結束代碼 10 |
| `PLS-W8301` | `network.rs` `ClockSkewLog`；`eval.rs` `clock_row` | 與 Kairos 相差 30 秒～5 分鐘（以 `hello_ack` 的 `server_time` 量測；只在等級改變或偏差再變化超過 1 分鐘時記錄） | `clock.skew`（WARN）；時鐘列 ⚠ |
| `PLS-E8302` | 同上 | 相差超過 5 分鐘 | `clock.skew`（ERROR，同時寫 stderr）；時鐘列 ✗。回到 30 秒以內時 `clock.skew -`（INFO） |

## 3. 日誌事件（`log-ux.md` §4）

| 事件 | 事件識別碼 | 程式位置 | 已實作 |
|---|---|---|---|
| `agent.start`、`agent.stop`、`agent.fatal` | 1000、1001、1020 | `main.rs` | ✓ |
| `config.warning` | 1003 | `main.rs`、`tls.rs` | ✓ |
| `identity.loaded`、`identity.created` | 1004、1005 | `main.rs` | ✓ |
| `sample.timeout` | 1030 | `system_info.rs` | ✓ |
| `conn.registered`、`conn.lost`、`conn.restored`、`conn.outage` | 2000、2001、2003、2004 | `network.rs` | ✓（`conn.restored` 附待補送筆數與丟棄筆數） |
| `tls.server_cert_rejected`、`auth.*`、`conn.superseded`、`proto.incompatible`、`server.full` | 2100～2301 | `network.rs` `disconnect_event` | ✓ |
| `report.rejected`、`report.skipped`、`server.unexpected` | 2400～2402 | `network.rs` | ✓ |
| `cert.expiring`、`cert.expired` | 3000、3003 | `tls.rs` | ✓ |
| `clock.skew` | 5000 | `network.rs` | ✓ |
| `log.retention_paused` | 5001 | `log_files.rs` | ✓ |
| `buffer.opened`、`buffer.unavailable`、`buffer.write_failed`、`buffer.full`、`buffer.expired`、`buffer.replay_done` | 5010～5015 | `main.rs`、`spool.rs`、`network.rs` | ✓ |
| `status.write_failed`、`status.write_restored` | 5020、5021 | `local_status.rs` | ✓（設計包 §26 補登） |
| `conn.retry`、`conn.retry_summary` | —、2002 | — | 待辦（§4） |
| `config.loaded`、`identity.clone_suspected`、`sysinfo.snapshot` | 1002、1006、1010 | — | 待辦 |
| `cert.renewed`、`cert.renew_failed`、`cert.renew_started`、`cert.grace_*` | 3001～3007 | — | 待辦（沒有自動更新憑證） |
| `install.*`、`enroll.*`、`admin.dangerous_action` | 4000～4112 | — | 待辦（沒有安裝程式與 `pulse enroll`） |

事件識別碼定義在 `codes.rs`（`codes::ALL`，單元測試檢查名稱與識別碼不重複、5010～5015 與設計包一致）。**Pulse 目前不寫 Windows 事件記錄**：需要安裝程式註冊事件來源 `Pulse`（MSI 尚未實作），屆時直接使用這份對照。

## 4. 待辦

### 4.1 尚未對應的代碼（53 個）

| 類別 | 代碼 | 原因 |
|---|---|---|
| 命令列、設定 | `PLS-E0004`、`PLS-E0103`、`PLS-E0202`、`PLS-E0401`、`PLS-E0402`、`PLS-W0102`、`PLS-W0104`、`PLS-W0105` | 沒有非互動危險操作、未知欄位偵測、`PULSE_CONFIG`；必填欄位與矛盾設定目前都歸在 `PLS-E0201`。`PLS-E0402`（設定檔無法讀取）在 `pulse status` 中刻意不視為錯誤（以服務回報的設定為準）；`--config` 指定的檔案不存在暫用 `PLS-E0002` |
| 憑證 | `PLS-W1001`、`PLS-E1104`、`PLS-E1106`、`PLS-E1107`、`PLS-W1111`、`PLS-I1112`、`PLS-E1113`、`PLS-E1114`、`PLS-E1406`、`PLS-W1407` | 啟動時的憑證／私鑰載入錯誤目前只有結束代碼 4 與 `agent.fatal`（訊息已具體），尚未逐一歸類；自動更新、過期續簽、CA 指紋與 TOFU 尚未實作 |
| 連線、註冊 | `PLS-W2006`、`PLS-E2007`、`PLS-E2008`、`PLS-E2106`、`PLS-E2202`、`PLS-E3001`～`PLS-E3005`、`PLS-W3105`、`PLS-E3106`、`PLS-W3107`、`PLS-E3301`～`PLS-E3306`、`PLS-I3307`、`PLS-I3308`、`PLS-E3309`、`PLS-W3310`、`PLS-E7101` | 註冊端點 8444、`pulse enroll`、除役、身分鎖檔、連線不穩定的判定尚未實作 |
| 儲存 | `PLS-E4001`、`PLS-E4301`、`PLS-W4302` | 磁碟空間檢查、`pulse diag` 尚未實作 |
| 平台、安裝 | `PLS-E8001`、`PLS-W8006`、`PLS-E8007`、`PLS-W8008`、`PLS-E8102`、`PLS-E8103`、`PLS-E8104`、`PLS-W8303` | 安裝程式、MSI、`pulse preflight`、時間同步狀態偵測尚未實作 |

### 4.2 日誌格式與其他

- `log-ux.md` §6 的完整格式（RFC 3339 帶時區的時間、5 欄寬等級、`event` 補到 18 欄、`key=value` 欄位、`log.format = "json"`、`run_id`／`seq`）與 §5 的重試摘要（`conn.retry_summary`：1／5／15／30 分、之後每小時）尚未實作；目前仍是每次斷線一行（WARN／節流的 ERROR）。
- systemd `Status:` 的文字仍是 0.2 的寫法（`無法註冊：certificate_rejected（…）`），尚未改為 `cli-spec.md` §2.2 的「標題（代碼，處理提示）」；`PLS-E4004` 已附在 `已註冊：…` 之後。
- `clock.skew` 的 `ERROR` 與 `status.write_failed` 的 `ERROR` 目前寫日誌與 stderr；Windows 事件記錄與服務狀態（`log-ux.md` §3）待 Windows 事件記錄實作時一併處理。
- Windows 事件記錄、服務描述欄位（`local-status.md` §5、§6）。
- `pulse status --all`、`--json-lines`、`--pause-on-exit`（「Pulse 狀態」捷徑）、`status.offline_error_after`（30 分鐘門檻固定）、`PULSE_LANG`。

## 5. 與設計包不一致之處

| 項目 | 設計包 | 實作 | 理由 |
|---|---|---|---|
| 狀態檔的備用位置 | `/var/lib/pulse/status.json`、`%ProgramData%\Pulse\status.json`，所有人可讀（`cli-spec.md` §4.1） | 依設計（2026-10-13 使用者決定）；另外在預設位置無法寫入時（沿用沒有 `StateDirectory=` 的舊 unit）改寫工作目錄的 `logs/status.json` 並以 `PLS-E8204` 警告 | 舊 unit 在 `ProtectSystem=strict` 下不能寫 `/var/lib/pulse`；不讓服務因狀態檔而啟動失敗。此時只有 root／`pulse` 群組讀得到 |
| `pulse status` 的結束代碼 10 | §1.3 只列 0／7／8／9／2 | 無法讀取狀態檔時為 10（`cli-style.md` §8「權限不足」，`--json` 的 `ok: false`、`error`） | 權限不足時無法判斷主機狀態，不應回 9 讓監控誤判為「嚴重」；SCCM 以 SYSTEM 執行，不會遇到 |
| 「身分」列 | 「…，2027-06-09 起自動更新」 | 沒有這一段；14 天內到期為 `PLS-W1110`（不會自動更新） | 這個版本沒有自動更新憑證 |
| `PLS-E2003` 的說明 | 原因寫出解析出的 IP；處理方式 `sudo pulse check-config --connect` | 不寫 IP；以 `nc -vz <主機> <埠>`（Windows `Test-NetConnection`）檢查 | 連線失敗時拿不到實際連線的位址；`pulse check-config` 尚未實作 |
| `PLS-E1105` 的處理方式 | `sudo pulse enroll --force --token <新 token>` | 請管理者重新簽發用戶端憑證、更新設定後重新啟動 | `pulse enroll` 尚未實作 |
| `PLS-E8003` 的處理方式 | `sudo pulse check-config` | 修正設定檔（寫出錯誤）或查看日誌 | `pulse check-config` 尚未實作 |
| 說明行 | `說明：pulse explain <代碼>` 指向本機完整說明 | `pulse explain` 只印標題並指向本文件 | 各代碼的完整說明頁尚未實作 |
| 「時鐘」列在被拒時 | 4.6 範例「與 Kairos 相差 0.3 秒」 | 未連線時一律加「（上次連線時量測）」（與 4.5 相同） | 被拒期間沒有新的量測 |
| 「離線（停用緩衝）」的寫法 | 4.5 註解：與第一行同一行；4.2 表格：第二行起 | 第二行起 | 依 4.2 表格 |
| `checks[].result` | `ok／warning／error／offline／pending／skipped` | 狀態檔過時時服務列為 `warning`（`PLS-W8004`），本機狀態另判為 `unknown` | 列舉值沒有 `unknown`；不新增列舉值 |
| `--json` 的補充欄位 | — | `connection` 另有 `server_verified`、`proto`、`features`、`last_error`；`reporting.buffer` 另有丟棄的取樣時間範圍、`retry_at` 等；`status_file.age_ms` | 只新增欄位，不影響既有欄位（`cli-style.md` §7.1） |

## 6. SCCM 指令碼核對（`design/pulse/sccm/*.ps1`）

- `Get-PulseHealth.ps1` 執行 `pulse.exe status --json`（不帶參數）：狀態檔在固定位置 `%ProgramData%\Pulse\status.json`（與設計包相同），SCCM 不需要額外參數；`pulse.toml` 依序找目前目錄與 `pulse.exe` 所在目錄。服務的寫入位置不可寫時，也會讀 `pulse.exe` 所在目錄的 `logs\status.json`。
- 讀取的欄位 `schema`（`pulse.status/*`）與 `data.status`（6 個值）一致；結束代碼 0／7／8／9 一致。不一致：無法讀取狀態檔時 `pulse status` 回 10、`data` 為 `null`，指令碼會落到「結束代碼不在預期範圍」而回報 `error`（SCCM 以 SYSTEM 執行，實務上不會發生）。
- `--json` 為純 ASCII、stderr 為空，30 秒逾時足夠（`pulse status` 不連線；`systemctl` 查詢上限 5 秒，Windows 為直接呼叫服務控制管理員）。
- `Detect-PulseInstalled.ps1` 讀服務 `Pulse` 的 ImagePath、`pulse.exe` 的檔案版本與 `install-result.json`：Pulse 目前沒有 MSI，也不產生 `install-result.json`，執行檔沒有設定 Windows 檔案版本資源（`FileMajorPart` 等為 0）；以 `$MinimumVersion = 0.2.0` 判斷時會判為未安裝。這是 MSI 與建置時版本資源的待辦，不影響 `pulse status`。
- 尚未在真的 SCCM 用戶端（Windows PowerShell 5.1、SYSTEM）上執行；本次只以 Windows 交叉編譯（clippy）確認服務控制管理員與主控台的呼叫可以編譯。
