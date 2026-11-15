# Pulse 錯誤代碼與日誌事件對照

本文件列出 Pulse 0.4 與 0.5（註冊通道）已對應到設計包代碼的地方（代碼 → 程式位置 → 觸發條件 → 在哪裡看得到），以及尚未實作的代碼。代碼的唯一來源是設計包 `design-system/copywriting.md` §3.3（Pulse 共 113 個，0.8.0 依 `decisions/INTEGRATION.md` §30.3 新增 4 個）；日誌事件依 `pulse/log-ux.md` §4。發佈包內附本文件，`pulse explain <代碼>` 印出代碼的標題並指向這裡。

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

## 2. 已對應的代碼（96 個，含 0.8.0 新增的 4 個：2.3 的 `PLS-W2108`、2.4 的 `PLS-I8009`〔指標擴充〕與 2.7 的 `PLS-E8105`、`PLS-E8106`〔Windows 服務〕，設計包 §30.3 正式分配；以及 0.9.0 的 21 個：2.8 的 `PLS-W2301`、`PLS-E2302`、`PLS-I8507`〔多伺服器位址與排空，設計包 §31.3 正式分配；`PLS-I8507` 原暫定為 `PLS-I8407`〕與 2.9 的 18 個〔集中設定、遠端升級，設計包 §31.3 正式分配〕）

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
| `PLS-E0401` | `diag.rs` `locate`（0.6） | `pulse check-config`／`pulse diag` 以 `-c` 指定的設定檔不存在 | `check-config` 的「設定檔」列 ✗，結束代碼 3 |
| `PLS-E4301` | `diag.rs` `run_diag`（0.6） | 無法寫出診斷包（輸出檔已存在、無法寫入、產生前檢查仍有疑似秘密） | stderr，結束代碼 1 |
| `PLS-W4302` | `diag.rs` `build`（0.6） | 診斷包的日誌超過 50 MB 上限，只收集較新的部分 | `pulse diag` 的輸出，結束代碼 8 |

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
| `PLS-W2108` | `inventory.rs` `InventoryConn::on_line` | Kairos 以 `invalid_report`（或其他非 `storage_unavailable` 的非致命 `error`）拒收 `inventory`：同樣的內容不再送，內容改變才再送（METRICS.md 第 6.3 節）。`storage_unavailable` 時 5 分鐘後重送：Kairos 暫時無法保存記 `PLS-W4101`；資產的速率限制（`inventory rate limit exceeded`，正常的 Pulse 不會發生）只記 `DEBUG`、沒有代碼（§30.3） | `inventory.rejected`（節流 10 分鐘） |
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
| `PLS-E8207`（**工程暫定號**，設計包尚未分配） | `main.rs` `run`（`state_dir.rs` 的檢查）；`status_cmd`、`remote::state_dir` 沿用同一個結果 | Windows：預設的狀態目錄 `%ProgramData%\Pulse` 沒有通過信任檢查——擁有者不是 SYSTEM／Administrators／TrustedInstaller、DACL 讓其他主體寫入（含 0.8.0～0.10.1 建立、繼承 `%ProgramData%` ACL 的目錄）、NULL DACL、是重新解析點，或不存在而無法以受保護的 ACL 建立（安全稽核 F4）。服務照常執行，狀態檔改寫 `logs\status.json`、集中設定快取改放 `logs\remote\`；`pulse status` 不讀該目錄 | `config.warning`（ERROR）；「設定」列 ⚠（處理方式：停止服務、`rd /s /q "%ProgramData%\Pulse"`、啟動服務） |
| `PLS-E8208`（**工程暫定號**，設計包尚未分配） | `main.rs` `run_agent`（`log_files::root_with_foreign_log_dir`） | Unix：常駐程式以 root 啟動，而 `logs/`（或它是符號連結時所指向的目錄；不存在時為工作目錄）屬於其他帳號（安全稽核 N15）。拒絕啟動，結束代碼 3（systemd 不重新啟動）。以服務帳號啟動，或把 `logs/`、`buffer/`、`identity/` 改為 root 擁有 | stderr |
| `PLS-W8005` | `system_info.rs` `Sampler::refresh_and_read`、`check_stuck`；`inventory.rs` `run_worker` | 系統資訊取樣逾時；收集主機資產超過 10 秒（這次不送 `inventory`，不影響 report） | `sample.timeout` |
| `PLS-I8009` | `system_info.rs` `report_limited` | 部分系統資訊因權限或平台限制無法取得，每個項目只記一次 `INFO`：系統序號（Linux 沒有 `/run/pulse-hw/product_serial`〔0.12.0 以前的 unit 為 `/run/pulse/product_serial`〕且不是 root、或 SMBIOS 沒有序號）、磁碟 I/O（讀不到 `/proc/diskstats`；Windows 讀不到效能計數器）、行程（只看得到自己的行程，例如 `ProtectProc=invisible`）。METRICS.md 第 4.5 節；`8006`～`8008` 保留給安裝前檢查 | `sysinfo.limited` |

### 2.5 `pulse status` 專用

| 代碼 | 程式位置 | 觸發條件 | 看得到的地方 |
|---|---|---|---|
| `PLS-E8002` | `status_cmd/eval.rs` `evaluate` | 找不到服務（systemd unit／Windows 服務），也沒有狀態檔 | 服務列 ✗，結束代碼 9 |
| `PLS-E8003` | 同上 | 服務管理員回報未執行（failed、inactive、正在重新啟動、已停止），或沒有服務管理員時狀態檔記錄的 PID 已不在 | 服務列 ✗（上次結束時間與結束代碼），其餘列為「最後已知狀態」 |
| `PLS-W8004` | 同上 | 服務在執行，但狀態檔超過 60 秒未更新（單調時鐘）、不存在或無法解析 | 服務列 ⚠，本機狀態「未知」，結束代碼 9（Nagios 3）；連線、回報、身分、時鐘列依最後已知內容判定並標示「（最後已知，N 分鐘前）」，沒有內容時略過 |
| `PLS-E8203` | `status_cmd::permission_denied` | 沒有權限讀取狀態檔 | stderr（`--json` 為 `error`），結束代碼 10（`--exit-codes=nagios` 為 3） |
| `PLS-W8301` | `network.rs` `ClockSkewLog`；`eval.rs` `clock_row` | 與 Kairos 相差 30 秒～5 分鐘（以 `hello_ack` 的 `server_time` 量測；只在等級改變或偏差再變化超過 1 分鐘時記錄） | `clock.skew`（WARN）；時鐘列 ⚠ |
| `PLS-E8302` | 同上 | 相差超過 5 分鐘 | `clock.skew`（ERROR，同時寫 stderr）；時鐘列 ✗。回到 30 秒以內時 `clock.skew -`（INFO） |

### 2.6 註冊與憑證更新（0.5，`crates/kairos-proto/ENROLL.md`）

8444 錯誤碼與 Pulse 代碼的對照以 `kairos-proto` 的 `EnrollErrorCode::pulse_code` 為準（ENROLL.md「錯誤碼」表）；Pulse 依端點再決定處置（`enroll/machine.rs`）。

| 代碼 | 程式位置 | 觸發條件 | 看得到的地方 |
|---|---|---|---|
| `PLS-E0004` | `enroll/cli.rs` | 非互動模式缺少 `--server`／`--token`、危險操作（`--force`、`--reset-identity`、`--cancel`）沒有 `--yes` | stderr、`--json` 的 `error`，結束代碼 2 |
| `PLS-E1104` | `enroll/flow.rs` `install` | 無法寫入取得的憑證或私鑰 | `pulse enroll` 結束代碼 4 |
| `PLS-E1107` | `enroll/flow.rs` `install`、`rotation.rs` | 取得的憑證與申請時的私鑰不成對（私鑰遺失） | `pulse enroll` 結束代碼 4；`cert.renew_failed` |
| `PLS-W1111` | `rotation.rs` | 8443 正常輪替失敗（`renew_requires_approval`、`invalid_csr`、`storage_unavailable`、以新憑證連線失敗）；1 → 12 小時退避 | `cert.renew_failed`（WARN）；`status.json` 的 `identity.renew_failed_code` |
| `PLS-I1112` | `rotation.rs` | 達輪替門檻，送出 `renew_request` | `cert.renew_started` |
| `PLS-E1113` | `enroll/machine.rs` `on_grace_error` | 過期續簽 `renew_grace_exceeded`；每 6 小時重試 | `cert.grace_exceeded`；身分列 ✗「已超過寬限期」、`identity.grace.renew_state = grace_exceeded` |
| `PLS-E1114` | 同上；`flow.rs` `build_grace_request` | `renew_identity_mismatch`、`invalid_csr`；讀不到主機指紋或舊私鑰不是 P-256 | `cert.grace_rejected`；連線列 ✗ |
| `PLS-E1406` | `enroll/trust.rs`、`flow.rs` `install`、`enroll/cli.rs` | 鏈中找不到指紋相符的 CA（不送出任何請求）；`issued` 的 `ca_pem` 與信任錨不同（不寫入）；非互動模式沒有指紋 | `pulse enroll` 結束代碼 5 |
| `PLS-W1407` | `enroll/cli.rs` | 以 TOFU 信任 CA（`--trust-on-first-use` 或互動確認） | `pulse enroll` 的 `data.issues[]`，結束代碼 8 |
| `PLS-E2007` | `enroll/machine.rs` | 8444 拒絕連線（多半是註冊端點為「關閉」）；過期續簽為 `grace_blocked` | `pulse enroll` 結束代碼 5；`cert.grace_renew_blocked`；連線列 ✗ |
| `PLS-E2008` | 同上 | `enroll_not_accepted`、`server-info` 的 `accepts.new_enrollment = false` | `enroll.not_accepted`；結束代碼 6 |
| `PLS-E2106` | 同上 | 404／405 且本文不是註冊通道的錯誤格式、`enroll_api` 沒有 `v1` | 結束代碼 5 |
| `PLS-E2202` | 同上 | 429 `rate_limited`、503 `enroll_busy`：依 `Retry-After` 排隊重送（不視為失敗） | `enroll.rate_limited`（WARN）；身分列「註冊端點忙碌，下次重試…」、`pending` |
| `PLS-E3001` | `enroll/cli.rs` | token 檢查碼不符（本機攔下、不送出） | 結束代碼 2 |
| `PLS-E3002`～`PLS-E3004` | `enroll/machine.rs`、`cli.rs` | `token_invalid`／`token_expired`／`token_exhausted`（`token-check` 時不產生任何檔案） | `enroll.token_error`；結束代碼 6；身分列 ✗ |
| `PLS-E3005` | 同上 | `source_blocked`：CLI 結束；服務等封鎖結束後重試 | `enroll.source_blocked`；結束代碼 6 |
| `PLS-W3107` | `decommission.rs` | 除役通知未完成（`unreachable`／`unsupported`／`cert_expired`／`cert_rejected`／`server_error`） | `install.decommission_unreported`；`pulse decommission` 結束代碼 8 |
| `PLS-E3301` | `enroll/cli.rs` | 已註冊，未加 `--force` | 結束代碼 11 |
| `PLS-E3302` | `enroll/machine.rs` `result_state` | 申請被拒絕（刪除未使用的私鑰） | `enroll.rejected`；身分列 ✗（含原因） |
| `PLS-E3303` | 同上 | 申請逾期、被新申請取代、輪詢時 `request_not_found` | `enroll.request_expired` |
| `PLS-E3304` | 同上 | `invalid_csr`；送出前的自我檢查（`validate_enroll_request`）不通過 | 結束代碼 6 |
| `PLS-E3305` | `enroll/cli.rs` | 已有進行中的申請，或另一個 `pulse enroll` 持有 `identity/.lock` | 結束代碼 11 |
| `PLS-E3306` | `enroll/flow.rs` | `enrollment.json` 缺少重送需要的資料 | 身分列 ✗ |
| `PLS-I3307` | `enroll/machine.rs` | 等待核准（新註冊、過期續簽） | `enroll.pending`；連線列 `…`，本機狀態「待核准」（結束代碼 7） |
| `PLS-I3308` | 同上 | 已核准、已取得憑證 | `enroll.approved` |
| `PLS-E3309` | 同上 | `pending_queue_full`：依 `Retry-After`（3600 秒）重試 | `enroll.queue_full` |
| `PLS-W3310` | `enroll/flow.rs` `withdraw` | 撤回未完成（`unreachable`／`unsupported`／`already_approved`／`signature_rejected`／`server_error`） | `install.withdrawal_unreported`；`pulse enroll --cancel`、`pulse decommission` 結束代碼 8 |
| `PLS-E7101` | `enroll/machine.rs` | `license_exhausted` | 結束代碼 6 |
| `PLS-E8302` | `enroll/machine.rs` | 註冊或過期續簽時 `clock_skew`：每 5 分鐘重送，偵測到本機時間被調整時立即 | `enroll.clock_rejected`／`cert.grace_renew_blocked`（ERROR）；身分列 ✗「相差…；校正後自動重送」；`clock.source = clock_skew` |
| `PLS-E1403`（8444） | `enroll/cli.rs` `connect_failure` | 註冊端點的伺服器憑證依本機時間已過期或尚未生效：以「忽略有效期間」再驗證一次、只送 `server-info`，訊息中寫出未驗證的 Kairos 時間 | `clock.source = server_info_unverified`、`valid: false` |

### 2.7 Windows 服務（0.8；設計包 §30.3 分配到子類別 81，工程暫定的 `8205`／`8206` 已改號）

| 代碼 | 程式位置 | 觸發條件 | 看得到的地方 |
|---|---|---|---|
| `PLS-E8105` | `winsvc.rs` `install`、`uninstall` | 服務控制管理員回報錯誤：沒有系統管理員權限、服務 `Pulse` 已存在（install）或不存在（uninstall）、無法啟動（`--start`） | stderr，`pulse service` 結束代碼 1 |
| `PLS-E8106` | `winsvc.rs` `run_service` | `pulse service run` 不是由服務控制管理員啟動（在主控台執行），或無法切換到 `--workdir` | stderr，結束代碼 2／1 |

### 2.8 多個伺服器位址與排空（0.9.0）

設計包 `decisions/INTEGRATION.md` §31.3（2026-10-19）正式分配：子類別 23「多伺服器位址與叢集連線」、85「叢集與高可用」（原暫定 84，84 已分配給遠端升級，排空由 `PLS-I8407` 改為 `PLS-I8507`）。`kairos/HA.md` 第 19 節同步。

| 代碼 | 程式位置 | 觸發條件 | 看得到的地方 |
|---|---|---|---|
| `PLS-W2301` | `network.rs` `run_with` | 設定了 `server.fallback_addresses`，某個候選位址在收到 `hello_ack` 之前失敗，已立即改試下一個（不增加退避）。每輪第一次失敗時判斷：`WARN` 每 10 分鐘最多一則，其餘記 `DEBUG` | `conn.failover`；本機指標 `pulse_connect_failovers_total` |
| `PLS-E2302` | `network.rs` `run_with` | 所有候選位址都無法連線（一般網路錯誤），依退避重試；日誌與 `status.json` 的 `connection.candidate_errors` 列出每個位址最後的錯誤。某個位址的錯誤需要人工處理（伺服器憑證不符、用戶端憑證被拒、伺服器的致命錯誤等）時改記該錯誤的代碼 | `conn.lost`；連線列 ○「所有候選位址都無法連線」（滿 30 分鐘 ✗），說明列出各位址 |
| `PLS-I8507`（鏡像 `KRS-I8507`） | `network.rs` `run_with`、`local_status.rs` `drained` | 協商了 feature `cluster`，Kairos 送 `drain`（節點排空或關機）：等在途的回應（最多 8 秒）後關閉，在 `reconnect_within_ms` 內的隨機延遲後改連下一個候選位址；不算連線失敗、不增加退避 | `conn.drain`（`INFO`）；連線列 ○「Kairos 節點正在排空，改連其他節點」，不列為問題；本機指標 `pulse_drains_total` |

設定值不合法（`fallback_addresses` 超過 16 個、IPv6 沒有方括號、連接埠不合法等）沿用 `PLS-E0201`。

### 2.9 集中設定與遠端升級（0.9.0）

設計包 `decisions/INTEGRATION.md` §31.3（2026-10-19）正式分配：子類別 05「集中設定」、84「遠端升級」（`crates/kairos-proto/REMOTE.md` 第 11 節）。程式位置為 `pulse/src/remote/` 下的檔案。`upgrade_status` 與 `config_status` 的 `code` 欄位也送出這些代碼，Kairos 端記錄的是鏡像的 `KRS-` 代碼。

| 代碼 | 程式位置 | 觸發條件 | 看得到的地方 |
|---|---|---|---|
| `PLS-W0501`（鏡像 `KRS-W0501`） | `config.rs` `apply_settings` | 下發的設定有任何白名單鍵的型別、範圍不合法或互相矛盾（含與本機的值合併後），整份拒絕、沿用上一版 | `config.rejected`；`config_status` 的 `state: rejected`、`errors`；「設定」列「集中設定：第 N 版被拒」 |
| `PLS-I0503`（鏡像 `KRS-I0503`） | `config.rs` `log_overridden` | 下發的鍵被 `pulse.toml` 或環境變數覆蓋（本機值與下發值不同） | `config.overridden`（覆蓋的鍵集合改變時一則）；`config_status` 的 `overridden` |
| `PLS-W0504`（鏡像 `KRS-W0504`） | `config.rs` `on_connect_failed`、`CentralConfig::new` | 套用後確認前連續 3 次無法完成註冊，或連續兩次在確認前重新啟動：已回復上一份可用的設定 | `config.rolled_back`；`config_status` 的 `state: rolled_back` |
| `PLS-I0505`（鏡像 `KRS-I0505`） | `config.rs` `CentralConfig::new` | `[remote_config] enabled = false` | `config.disabled`（啟動時一則）；停用後第一次連線的 `config_status` |
| `PLS-E0506` | `config.rs` `CacheFile::load`、`persist` | 遠端設定的快取（`<狀態目錄>/remote/config.json`）損毀或無法寫入：改用本機設定；無法寫入時新的版本不套用（`rejected`） | `config.cache_unavailable`（ERROR）；`pulse check-config --with-remote` 的警告 |
| `PLS-E8401`（鏡像 `KRS-E8401`） | `upgrade/mod.rs` `verify_failed`；`upgrade/helper.rs` | 發佈包的簽章驗證失敗（簽署者不在信任錨中、簽章無效、namespace 不符、`SHA256SUMS` 被竄改）：不替換、刪除暫存、不重試同一個提議 | `upgrade.verify_failed`（ERROR，同時寫 stderr）；`upgrade_status` 的 `failed` |
| `PLS-E8402`（鏡像 `KRS-W8402`） | 同上 | 發佈檔的雜湊、檔名、平台或版本與簽署的 `SHA256SUMS`、清單、提議不符；下載後 SHA-256 連續 3 次不符；提議的格式不合法（`declined`）；輔助程式複製時遇到符號連結、硬連結或超大檔案；輔助程式：`request.json` 的 `from_version`、`method` 與它自己的不符（`from_version_mismatch`、`method_mismatch`）；0.10.0：root 的交易紀錄或進行中交易的指標損毀、遺失或與系統的實況矛盾（`state_unusable`、`transaction_pointer_unusable`，`detail` 以 `manual check required` 結尾：不替換也不回滾，檢查主機後刪除 `/var/lib/pulse-upgrade/active.json` 與對應的工作目錄才能再遠端升級；REMOTE.md 第 17.9.5 節） | `upgrade.verify_failed`；`upgrade_status` 的 `failed`（格式不合法為 `declined`） |
| `PLS-E8403` | `install.rs` `load_trust_anchor`；`upgrade/mod.rs` | 信任錨 `allowed_signers` 不存在、不是一般檔案、不是 root 擁有、群組或其他人可寫，或沒有可用於 `kairos-pulse-release` 的 `ssh-ed25519` 金鑰：遠端升級停用（`install.upgrade = "no_trust_anchor"`） | `upgrade.trust_anchor_unavailable`（啟動時一則 ERROR）；`pulse upgrade status` |
| `PLS-W8404`（鏡像 `KRS-W8404`） | `upgrade/mod.rs` `log_download` | 下載中斷、段落不符、暫存檔無法寫入、Kairos 回應錯誤：續傳重試中 | `upgrade.download_failed`（節流 10 分鐘） |
| `PLS-E8405`（鏡像 `KRS-E8405`） | `upgrade/helper.rs` `prepare`、`apply`；`install.rs` `detect`（0.10.0 的預檢） | 新版預檢失敗：無法解開、`pulse --version` 不是目標版本、`pulse check-config --with-remote` 結束代碼不是 0／8、`pulse.toml` 無法載入；0.10.0：安裝目錄、`/var/lib/pulse-upgrade`、交易的工作目錄或它們的上層目錄不屬於 root、可由群組或其他人寫入或是符號連結（`unsafe_path: <路徑>: <原因>`，REMOTE.md 第 17.9.7 節）；沒有變更 | `upgrade.failed`；`upgrade_status` 的 `failed` |
| `PLS-E8406`（鏡像 `KRS-E8406`） | `upgrade/helper.rs` `replace`；`upgrade/mod.rs` `write_request` | 替換失敗（檔案操作、`dpkg`／`rpm` 失敗或鎖住超過 10 分鐘、`request.json` 無法寫入）：已換回原本的執行檔。0.10.0：替換前寫不進交易紀錄或指標（`state_not_saved`）、工作目錄裡驗證過的複本與紀錄的雜湊不符（`verified_copy_changed`）時不開始替換；系統已被改過時換回 `pulse.prev` 並確認舊版恢復，`detail` 以 `restored` 結尾（不恢復時為 `PLS-E8408`） | `upgrade.failed`；`journalctl -u pulse-upgrade` |
| `PLS-W8407`（鏡像 `KRS-W8407`） | `upgrade/helper.rs` `confirm_rollback` | 健康確認失敗（逾時、崩潰重啟、結束代碼 2／3／4），已自動回滾且舊版恢復 | `upgrade.rolled_back`；「升級」列 ⚠「已回滾到 X」 |
| `PLS-W8418`（鏡像 `KRS-W8418`；**暫定號**，ADR-0003，正式分配由設計包負責） | `upgrade/helper.rs` `confirm_rollback` | 降級回滾（0.12.0 起）：健康確認失敗後舊版已恢復，但 deb／rpm 只換回了執行檔（沒有合格的回滾套件、套件回滾失敗，或回滾套件的簽署金鑰已撤銷），套件資料庫與實際的執行檔不一致。`upgrade_status.state` 仍是 `rolled_back`，`detail` 以 `degraded:<原因>,…` 開頭 | `upgrade.rolled_back`（WARN，附修復方式）；「升級」列 ⚠「需要修復套件」直到重新安裝套件或下一次成功的升級，`pulse status` 結束代碼 8；`pulse upgrade status` 顯示修復的命令 |
| `PLS-E8408`（鏡像 `KRS-E8408`） | `upgrade/helper.rs` `rollback_failed` | 回滾後舊版也不健康，或無法換回：保留所有暫存與紀錄，不再動作 | `upgrade.rollback_failed`（ERROR，同時寫 stderr）；「升級」列 ✗，`pulse status` 結束代碼 9 |
| `PLS-I8409`（鏡像 `KRS-I8409`） | `upgrade/mod.rs` `decline`；`helper.rs` | `[upgrade] enabled = false`（`pulse.toml` 或環境變數；輔助程式只看 `pulse.toml`） | `upgrade.declined`（INFO） |
| `PLS-W8410`（鏡像 `KRS-W8410`） | `upgrade/mod.rs`；`helper.rs` | 目標版本低於目前版本，而推送或本機的 `allow_downgrade` 不允許 | `upgrade.declined` |
| `PLS-W8411`（鏡像 `KRS-W8411`） | `upgrade/mod.rs` `on_offer`；`cli.rs` | 不支援的安裝方式或條件：缺少輔助程式、多實例、容器、未知安裝方式、外部 URL（`url_not_allowed`，0.10.0）、低於清單的 `min_upgrade_from`、沒有清單 | `upgrade.declined`；`pulse upgrade status` |
| `PLS-W8412`（鏡像 `KRS-W8412`） | `upgrade/mod.rs` `on_offer` | 狀態目錄所在的檔案系統可用空間小於 `3 × 發佈檔大小 + 64 MiB`，或暫存目錄無法建立 | `upgrade.declined` |
| `PLS-I8416`（鏡像 `KRS-I8416`） | `upgrade/helper.rs` `after_replace`；`upgrade/mod.rs` `poll_result` | 健康確認通過，已升級 | `upgrade.completed`；「升級」列「已升級到 X」 |

## 3. 日誌事件（`log-ux.md` §4）

| 事件 | 事件識別碼 | 程式位置 | 已實作 |
|---|---|---|---|
| `agent.start`、`agent.stop`、`agent.fatal` | 1000、1001、1020 | `main.rs` | ✓ |
| `config.warning` | 1003 | `main.rs`、`tls.rs` | ✓ |
| `identity.loaded`、`identity.created` | 1004、1005 | `main.rs` | ✓ |
| `sample.timeout` | 1030 | `system_info.rs`、`inventory.rs` | ✓（0.8.0 起含主機資產的收集逾時） |
| `sysinfo.snapshot` | 1010 | `system_info.rs` `Extras::log_skipped` | 部分（0.8.0）：只記超過每類上限而略過的掛載點、磁碟 I/O 裝置、網路介面（`skipped_*`），第一次取樣時一則、之後每 24 小時最多一則；啟動時的完整系統資訊仍是沒有事件的 INFO 行 |
| `sysinfo.limited`、`inventory.rejected` | 〔待分配〕 | `system_info.rs` `report_limited`、`inventory.rs` | ✓（0.8.0；設計包沒有，事件名稱與識別碼待設計組確認，見 METRICS.md 第 14 節） |
| `conn.registered`、`conn.lost`、`conn.restored`、`conn.outage` | 2000、2001、2003、2004 | `network.rs` | ✓（`conn.restored` 附待補送筆數與丟棄筆數） |
| `conn.failover`、`conn.drain` | 2005、2007 | `network.rs` `run_with` | ✓（0.9.0；`PLS-W2301`、`PLS-I8507`；設計包 `pulse/log-ux.md` §4.2） |
| `tls.server_cert_rejected`、`auth.*`、`conn.superseded`、`proto.incompatible`、`server.full` | 2100～2301 | `network.rs` `disconnect_event` | ✓ |
| `report.rejected`、`report.skipped`、`server.unexpected` | 2400～2402 | `network.rs` | ✓ |
| `cert.expiring`、`cert.expired` | 3000、3003 | `tls.rs` | ✓ |
| `clock.skew` | 5000 | `network.rs` | ✓ |
| `log.retention_paused` | 5001 | `log_files.rs` | ✓ |
| `buffer.opened`、`buffer.unavailable`、`buffer.write_failed`、`buffer.full`、`buffer.expired`、`buffer.replay_done` | 5010～5015 | `main.rs`、`spool.rs`、`network.rs` | ✓ |
| `status.write_failed`、`status.write_restored` | 5020、5021 | `local_status.rs` | ✓（設計包 §26 補登） |
| `config.applied`、`config.rejected`、`config.rolled_back`、`config.overridden`、`config.cache_unavailable`、`config.disabled` | 6000～6005 | `remote/config.rs` | ✓（0.9.0；設計包 `pulse/log-ux.md` §4.6） |
| `upgrade.offered`～`upgrade.trust_anchor_unavailable` | 6100～6110 | `remote/upgrade/mod.rs`（Pulse 端）；輔助程式 `pulse upgrade apply` 只寫 stderr（journal）與 `/var/lib/pulse-upgrade/<id>/upgrade.log`，不使用事件識別碼 | ✓（0.9.0；`upgrade.completed`、`upgrade.rolled_back`、`upgrade.rollback_failed`、`upgrade.failed` 由替換後的 Pulse 讀 `result.json` 時記錄） |
| `conn.retry`、`conn.retry_summary` | —、2002 | — | 待辦（§4） |
| `config.loaded`、`identity.clone_suspected` | 1002、1006 | — | 待辦 |
| `cert.renewed`、`cert.renew_failed`、`cert.renew_started`、`cert.grace_*` | 3001～3007 | `main.rs` `connection_loop`、`rotation.rs`、`enroll/service.rs` | ✓（0.5） |
| `install.decommission_unreported`、`install.withdrawal_unreported` | 4002、4005 | `decommission.rs`、`enroll/cli.rs` | ✓（`pulse decommission`、`pulse enroll --cancel`；沒有初始化日誌時只寫 stderr） |
| `enroll.submitted`～`enroll.request_expired`、`enroll.rate_limited`～`enroll.clock_rejected` | 4100～4105、4108～4112 | `enroll/service.rs` `ServiceObserver` | ✓（服務在背景接續申請時；`pulse enroll` 只輸出到終端機） |
| `admin.dangerous_action` | 4107 | `enroll/cli.rs` | 部分：`pulse enroll` 不初始化日誌檔，事件沒有寫進 `logs/` |
| `install.completed`、`install.failed`、`install.uninstalled`、`install.cancelled`、`enroll.identity_conflict` | 4000、4001、4003、4004、4106 | — | 待辦（沒有安裝程式；身分衝突沿用 `conn.superseded`） |

事件識別碼定義在 `codes.rs`（`codes::ALL`，單元測試檢查名稱與識別碼不重複、5010～5015 與設計包一致）。**Pulse 目前不寫 Windows 事件記錄**：需要安裝程式註冊事件來源 `Pulse`（MSI 尚未實作），屆時直接使用這份對照。

## 4. 待辦

### 4.1 尚未對應的代碼（20 個）

| 類別 | 代碼 | 原因 |
|---|---|---|
| 命令列、設定 | `PLS-E0103`、`PLS-E0202`、`PLS-E0402`、`PLS-W0102`、`PLS-W0104`、`PLS-W0105` | 未知欄位偵測、`PULSE_CONFIG`；必填欄位與矛盾設定目前都歸在 `PLS-E0201`。`PLS-E0402`（設定檔無法讀取）在 `pulse status` 中刻意不視為錯誤（以服務回報的設定為準），`pulse check-config`／`diag` 則以它表示無法讀取；`pulse status --config` 指定的檔案不存在暫用 `PLS-E0002`（`check-config`／`diag` 為 `PLS-E0401`） |
| 憑證 | `PLS-W1001`、`PLS-E1106` | Kairos 伺服器憑證即將到期的提醒、加密私鑰的專屬代碼尚未實作；啟動時的憑證／私鑰載入錯誤仍只有結束代碼 4 與 `agent.fatal`（`PLS-E1104`、`PLS-E1107` 目前只用於註冊流程） |
| 連線、身分 | `PLS-W2006`、`PLS-W3105`、`PLS-E3106` | 連線不穩定的判定、`agent.id.host` 的複製主機偵測、常駐行程的身分鎖檔尚未實作 |
| 儲存 | `PLS-E4001` | 磁碟空間檢查尚未實作 |
| 平台、安裝 | `PLS-E8001`、`PLS-W8006`、`PLS-E8007`、`PLS-W8008`、`PLS-E8102`、`PLS-E8103`、`PLS-E8104`、`PLS-W8303` | 安裝程式、MSI、`pulse preflight`、時間同步狀態偵測尚未實作 |

### 4.2 日誌格式與其他

- 0.6 已實作 `log-ux.md` §6 的 RFC 3339 帶時區的時間、`log.format = "json"`（`v`、`ts`、`level`、`event`、`code`、`msg`、`agent_id`、`instance`、`run_id`、`seq`，事件專屬欄位在 `fields` 物件中）、`log.timezone` 與 syslog 外送；`text` 仍保留 0.5 的 `[等級] <event> <code> 訊息` 版面（未改為 5 欄寬等級、`event` 補到 18 欄）。§5 的重試摘要（`conn.retry_summary`：1／5／15／30 分、之後每小時）、`log.lang`、`log.stderr`、`log.outage_error_after` 尚未實作；目前仍是每次斷線一行（WARN／節流的 ERROR）。
- systemd `Status:` 的文字仍是 0.2 的寫法（`無法註冊：certificate_rejected（…）`），尚未改為 `cli-spec.md` §2.2 的「標題（代碼，處理提示）」；`PLS-E4004` 已附在 `已註冊：…` 之後。
- `clock.skew` 的 `ERROR` 與 `status.write_failed` 的 `ERROR` 目前寫日誌與 stderr；Windows 事件記錄與服務狀態（`log-ux.md` §3）待 Windows 事件記錄實作時一併處理。
- Windows 事件記錄、服務描述欄位（`local-status.md` §5、§6）。
- `pulse status --all`、`--json-lines`、`--pause-on-exit`（「Pulse 狀態」捷徑）、`status.offline_error_after`（30 分鐘門檻固定）、`PULSE_LANG`。

## 5. 與設計包不一致之處

設計包 §26（`decisions/INTEGRATION.md`，2026-10-13）已把 `pulse status` 的結束代碼 10（`--exit-codes=nagios` 為 3）、`checks[]` 不新增 `unknown`（狀態檔過時時依最後已知內容判定並標示「（最後已知，N 分鐘前）」，沒有內容時 `skipped`）、`status.write_failed`（5020）與 `clock.skew` 的層級補進規格，與實作一致，不再列於下表。

| 項目 | 設計包 | 實作 | 理由 |
|---|---|---|---|
| 狀態檔的備用位置 | `/var/lib/pulse/status.json`、`%ProgramData%\Pulse\status.json`，所有人可讀（`cli-spec.md` §4.1） | 依設計（2026-10-13 使用者決定）；另外在預設位置無法寫入時（沿用沒有 `StateDirectory=` 的舊 unit）改寫工作目錄的 `logs/status.json` 並以 `PLS-E8204` 警告 | 舊 unit 在 `ProtectSystem=strict` 下不能寫 `/var/lib/pulse`；不讓服務因狀態檔而啟動失敗。此時只有 root／`pulse` 群組讀得到 |
| 「身分」列 | 「…，2027-06-09 起自動更新」 | 沒有這一段；14 天內到期為 `PLS-W1110`（不會自動更新） | 這個版本沒有自動更新憑證 |
| `PLS-E2003` 的說明 | 原因寫出解析出的 IP；處理方式 `sudo pulse check-config --connect` | 不寫 IP；以 `nc -vz <主機> <埠>`（Windows `Test-NetConnection`）檢查 | 連線失敗時拿不到實際連線的位址；`pulse check-config` 尚未實作 |
| `PLS-E1105` 的處理方式 | `sudo pulse enroll --force --token <新 token>` | 請管理者重新簽發用戶端憑證、更新設定後重新啟動 | `pulse status` 的三段式尚未依「身分是否由 pulse enroll 管理」切換（0.5 待辦）；`pulse enroll` 的錯誤輸出已依設計包 |
| `pulse.toml` 的寫入時機 | `issued` 時（設計包 §7） | Kairos **受理**申請（`pending_approval`／`issued`）或交給服務在背景重送時就寫入 | `/opt/pulse` 對服務唯讀，服務在背景取得憑證時無法改寫 `pulse.toml`；先寫好指向 `identity/` 的路徑，服務取得憑證後重新讀設定即可連線 |
| token 是否寫入磁碟 | 不寫入（`enrollment.json` 只記前 4 碼） | 申請**建立前**（429／503 排隊、`clock_skew`）暫存在 `enrollment.json`（0600），Kairos 受理或無法再使用時立即刪除 | 設計包要求服務在背景重送（§5.5a、§5.12），沒有 token 無法重送 |
| 申請被拒絕後的 `enrollment.json` | 刪除私鑰與 `enrollment.json` | 刪除私鑰與輪詢憑據，保留 `enrollment.json`（`state: rejected`） | 讓服務與 `pulse status` 在重新啟動後仍能顯示 `PLS-E3302`，並在新的 `pulse enroll` 之前不連線 |
| 安裝程式的通知命令 | 未定義命令名稱（§5.10、§5.11 只寫「安裝程式」） | `pulse decommission --initiated-by <…>` | 需要一個命令讓 MSI／安裝腳本／`postrm` 呼叫；名稱待設計包負責人確認 |
| `PLS-E8003` 的處理方式 | `sudo pulse check-config` | 修正設定檔（寫出錯誤）或查看日誌 | `pulse check-config` 尚未實作 |
| 說明行 | `說明：pulse explain <代碼>` 指向本機完整說明 | `pulse explain` 只印標題並指向本文件 | 各代碼的完整說明頁尚未實作 |
| 「時鐘」列在被拒時 | 4.6 範例「與 Kairos 相差 0.3 秒」 | 未連線時一律加「（上次連線時量測）」（與 4.5 相同） | 被拒期間沒有新的量測 |
| 「離線（停用緩衝）」的寫法 | 4.5 註解：與第一行同一行；4.2 表格：第二行起 | 第二行起 | 依 4.2 表格 |
| `--json` 的補充欄位 | — | `connection` 另有 `server_verified`、`proto`、`features`、`last_error`；`reporting.buffer` 另有丟棄的取樣時間範圍、`retry_at` 等；`status_file.age_ms` | 只新增欄位，不影響既有欄位（`cli-style.md` §7.1） |

## 6. SCCM 指令碼核對（`design/pulse/sccm/*.ps1`）

- `Get-PulseHealth.ps1` 執行 `pulse.exe status --json`（不帶參數）：狀態檔在固定位置 `%ProgramData%\Pulse\status.json`（與設計包相同），SCCM 不需要額外參數；`pulse.toml` 依序找目前目錄與 `pulse.exe` 所在目錄。服務的寫入位置不可寫時，也會讀 `pulse.exe` 所在目錄的 `logs\status.json`。
- 讀取的欄位 `schema`（`pulse.status/*`）與 `data.status`（6 個值）一致；結束代碼 0／7／8／9 一致。不一致：無法讀取狀態檔時 `pulse status` 回 10、`data` 為 `null`，指令碼會落到「結束代碼不在預期範圍」而回報 `error`（SCCM 以 SYSTEM 執行，實務上不會發生）。
- `--json` 為純 ASCII、stderr 為空，30 秒逾時足夠（`pulse status` 不連線；`systemctl` 查詢上限 5 秒，Windows 為直接呼叫服務控制管理員）。
- `Detect-PulseInstalled.ps1` 讀服務 `Pulse` 的 ImagePath、`pulse.exe` 的檔案版本與 `install-result.json`：Pulse 目前沒有 MSI，也不產生 `install-result.json`，執行檔沒有設定 Windows 檔案版本資源（`FileMajorPart` 等為 0）；以 `$MinimumVersion = 0.2.0` 判斷時會判為未安裝。這是 MSI 與建置時版本資源的待辦，不影響 `pulse status`。
- 尚未在真的 SCCM 用戶端（Windows PowerShell 5.1、SYSTEM）上執行；本次只以 Windows 交叉編譯（clippy）確認服務控制管理員與主控台的呼叫可以編譯。
