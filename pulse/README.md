# Pulse

輕量級的**地端系統監控代理程式（on-premise monitoring agent）**，以 Rust 撰寫。

Pulse 在本機收集系統資訊（CPU、記憶體），透過**一條長期維持的 TLS 加密連線**向中央伺服器 Kairos 註冊、定期送出心跳（heartbeat）與系統資訊回報（report）。連線中斷時會自動以指數退避無限重連。

## 功能特色

- 🖥️ 收集系統資訊：CPU 使用率、記憶體（啟動時另印出一次完整資訊：磁碟、網路、元件溫度）
- 🔐 單一長 TLS 連線（rustls），NDJSON 協定（一行一則 JSON），預設**驗證伺服器憑證**（作業系統信任庫 + 自訂 CA）
- 🪪 每台主機有固定的 `agent_id`（UUID v4，存於 `agent.id`），換主機名稱也不影響識別
- 💓 每秒送出 heartbeat，伺服器據此判斷上線 / 離線
- 🔁 斷線自動重連：指數退避 + ±20% 抖動、有上限、不限次數
- 🔑 可選 mTLS 客戶端憑證：以憑證 SAN 綁定 `agent_id`，防止冒用；支援中繼 CA 鏈與 RSA / ECDSA / Ed25519 私鑰
- ⚙️ 設定三層疊加：嵌入預設值 → `pulse.toml` → `PULSE__*` 環境變數
- 📝 日誌同時輸出到終端與 `logs/pulse-YYYY-MM-DD.log`（按日分檔、自動清除過期檔）

## 系統需求

- [Rust](https://www.rust-lang.org/) 1.99（edition 2024）；repo 內的 `rust-toolchain.toml` 會自動選用 1.99.0
- 搭配的伺服器：Kairos（同一個 repo 群組中的 `Kairos` 專案）
- 不需要 OpenSSL 或其他 C 函式庫：TLS 使用 rustls（ring 加密後端）

## 快速開始（搭配 Kairos，本機測試）

### 1. 產生含 SAN 的自簽憑證

```bash
MSYS_NO_PATHCONV=1 openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout key.pem -out cert.pem -days 365 -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
  -addext "basicConstraints=critical,CA:FALSE"
```

> `MSYS_NO_PATHCONV=1` 是給 Windows 的 Git Bash 用的，避免 `/CN=localhost` 被誤轉成路徑；Linux / macOS 可省略。
> 現代 TLS 實作只看 SAN（subjectAltName），不看 CN，所以 SAN 不能省。
> **`basicConstraints=critical,CA:FALSE` 也不能省**：`openssl req -x509` 預設會把憑證標記為 CA（`CA:TRUE`），而 rustls 不接受 CA 憑證直接當伺服器憑證使用，會以 `CaUsedAsEndEntity` 拒絕握手（Pulse 日誌會提示加上這個參數）。

把 `cert.pem`、`key.pem` 交給 Kairos 使用（詳見 Kairos 的 README）。

### 2. 啟動 Kairos

依 Kairos README 啟動，預設監聽 `127.0.0.1:8080`。

### 3. 啟動 Pulse，信任該自簽憑證

在 Pulse 的工作目錄建立 `pulse.toml`：

```toml
[server]
ca_cert = "path/to/cert.pem"   # 指向步驟 1 產生的 cert.pem
```

或改用環境變數：

```bash
PULSE__SERVER__CA_CERT=path/to/cert.pem cargo run
```

成功時日誌會出現：

```
[INFO] 已建立 TLS 加密連線：127.0.0.1:8080（domain=localhost，TLSv1_3）
[INFO] 已向伺服器註冊（agent_id=…，伺服器離線判定 5000 ms）
```

> 若伺服器尚未啟動，日誌會出現「TCP 連線失敗」並依退避策略重連，屬正常現象。

## 私有 CA（正式環境建議）

正式環境不要讓每台 Pulse 各自信任某一張伺服器憑證，而是建立一個私有 CA，用它簽發伺服器憑證，Pulse 只信任 CA：

```bash
# 1. 建立 CA（ca.key 請離線保存，不要放在伺服器上）
MSYS_NO_PATHCONV=1 openssl req -x509 -newkey rsa:4096 -nodes \
  -keyout ca.key -out ca.pem -days 3650 -subj "/CN=Pulse Private CA"

# 2. 產生伺服器金鑰與 CSR（SAN 填實際的伺服器域名 / IP）
MSYS_NO_PATHCONV=1 openssl req -newkey rsa:2048 -nodes \
  -keyout key.pem -out server.csr -subj "/CN=kairos.example.com" \
  -addext "subjectAltName=DNS:kairos.example.com" \
  -addext "basicConstraints=critical,CA:FALSE" -addext "extendedKeyUsage=serverAuth"

# 3. 用 CA 簽發伺服器憑證（-copy_extensions copyall 需 OpenSSL 3.0+，用來保留 CSR 中的 SAN 等擴充）
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
  -out cert.pem -days 825 -copy_extensions copyall
```

- Kairos 使用 `cert.pem` / `key.pem`。
- 每台 Pulse 設定 `server.ca_cert = "ca.pem"`、`server.domain = "kairos.example.com"`（以 IP 連線時填 IP，憑證 SAN 也要含該 IP）。
- 伺服器憑證到期時只要用 CA 重簽，Pulse 端不必改設定。
- `ca.key` 是整個信任鏈的根，務必離線保存。
- 由公開 CA 簽發的伺服器憑證不需要設定 `ca_cert`：Pulse 會自動信任作業系統的信任庫。

## 啟用 mTLS（客戶端憑證）

Kairos 設定了客戶端 CA（`auth.client_ca`）時會要求雙向 TLS：每台 Pulse 必須出示由該 CA 簽發、且 **SAN 為 `DNS:<自己的 agent_id>`**、EKU 含 `clientAuth` 的客戶端憑證，否則無法連線。這樣只知道某台主機 `agent_id` 的人，無法冒用它，也無法把它踢下線。詳細規則見 [PROTOCOL.md](PROTOCOL.md) 的「身分驗證（mTLS）」。

### 1. 在 Pulse 主機上取得 agent_id

```bash
pulse --print-agent-id
# 6f1c0e4a-1b2c-4d3e-9f00-112233445566
```

- stdout 只有 ID 一行，方便腳本使用；不連線、不寫日誌。
- `agent.id` 不存在時會先產生（提示訊息寫在 stderr）；內容不合法時以非 0 結束。
- 請在 Pulse 實際執行的工作目錄下執行（或用相同的 `agent.id_file` 設定），才會讀到同一份 `agent.id`。

### 2. 由管理者簽發客戶端憑證

完整流程以 Kairos README 為準，精簡版如下（`<id>` 換成上一步的 agent_id）：

```bash
# 管理者：建立 client CA（只需一次；client-ca.key 請離線保存）
MSYS_NO_PATHCONV=1 openssl req -x509 -newkey rsa:4096 -nodes \
  -keyout client-ca.key -out client-ca.pem -days 3650 -subj "/CN=Pulse Client CA"

# 產生私鑰與 CSR（RSA 或 EC 皆可，例如 EC P-256 改用：-newkey ec -pkeyopt ec_paramgen_curve:P-256）
MSYS_NO_PATHCONV=1 openssl req -new -newkey rsa:2048 -nodes \
  -keyout client.key -out client.csr -subj "/CN=<id>" \
  -addext "subjectAltName=DNS:<id>" -addext "extendedKeyUsage=clientAuth"

# 用 client CA 簽發（-copy_extensions copyall 需 OpenSSL 3.0+，保留 CSR 中的 SAN 與 EKU）
openssl x509 -req -in client.csr -CA client-ca.pem -CAkey client-ca.key -CAcreateserial \
  -out client.pem -days 825 -copy_extensions copyall
```

`client-ca.pem` 交給 Kairos（`auth.client_ca`）；`client.pem`、`client.key` 交給該台 Pulse。

若客戶端憑證是由**中繼 CA** 簽發，把中繼 CA 接在客戶端憑證後面即可（`cat client.pem intermediate.pem > client-chain.pem`），Pulse 會在握手時送出整條鏈，伺服器只需信任根 CA。

### 3. 設定 Pulse

```toml
# pulse.toml
[server]
ca_cert = "ca.pem"            # 信任伺服器憑證的 CA（與 mTLS 無關，但通常一起設定）
client_cert = "client.pem"    # 客戶端憑證（可在後面接上中繼 CA）
client_key = "client.key"     # 對應私鑰
```

或使用環境變數 `PULSE__SERVER__CLIENT_CERT`、`PULSE__SERVER__CLIENT_KEY`。

- 兩者必須同時設定或同時留空，只設定一個時啟動即報錯。
- 私鑰可為 PKCS#8（`BEGIN PRIVATE KEY`）、PKCS#1（`BEGIN RSA PRIVATE KEY`）或 SEC1（`BEGIN EC PRIVATE KEY`）；演算法支援 RSA、ECDSA P-256 / P-384、Ed25519。不支援有密碼的私鑰。
- 啟動時就會檢查，任何一項不符都立即以非 0 結束並說明原因：
  - 檔案讀不到、找不到憑證或私鑰、格式錯誤
  - **私鑰與憑證不成對**（私鑰的公鑰與憑證不符）
  - **憑證的 SAN 不含 `DNS:<本機 agent_id>`**（伺服器一定會以 `auth_failed` 拒絕，所以不必連線就先報錯）
- 成功載入時日誌會出現 `已載入客戶端憑證：<路徑>（憑證鏈 N 張；私鑰：<路徑>）`。

**agent.id 與憑證綁定**：重新產生 `agent.id`（刪除檔案、或複製到新主機後重產）就會得到新的 agent_id，必須重新簽發憑證；否則 Pulse 啟動時就會因 SAN 不符而報錯。

### 連線失敗時的提示

TLS 失敗時，Pulse 會在重連日誌中附上具體原因，例如：

| 情況 | 日誌提示 |
|---|---|
| 伺服器要求客戶端憑證但未設定 | 伺服器要求客戶端憑證（mTLS），但未設定 server.client_cert / server.client_key |
| 伺服器不信任此客戶端憑證（簽發 CA 不對、缺中繼 CA、EKU 不含 clientAuth） | 伺服器不信任此客戶端憑證…／伺服器可能不接受此客戶端憑證… |
| 伺服器表示客戶端憑證已撤銷或過期 | 伺服器表示此客戶端憑證已被撤銷（或過期），請聯絡管理者重新簽發 |
| 伺服器憑證的簽發者不受信任 | 伺服器憑證不受信任…請以 server.ca_cert 指定 |
| 伺服器憑證名稱與 `server.domain` 不符 | 伺服器憑證的名稱與 server.domain「…」不符 |
| 伺服器憑證被標記為 CA | 伺服器憑證被標記為 CA（basicConstraints CA:TRUE）…加上 basicConstraints=critical,CA:FALSE |

TLS 1.3 下伺服器在握手**之後**才驗證客戶端憑證，拒絕時送出的 alert 可能因連線被立即重設而收不到。Pulse 會記錄伺服器在握手時是否送出了客戶端憑證要求（CertificateRequest），因此即使沒收到 alert 也能指出是客戶端憑證的問題。

伺服器以錯誤碼區分兩種身分驗證失敗，Pulse 依錯誤碼給出提示：

| 錯誤碼 | 情境 | 日誌提示（`ERROR`） |
|---|---|---|
| `auth_failed` | hello 的 `agent_id` 與客戶端憑證的 SAN 不符 | 客戶端憑證的 SAN 必須是 `DNS:<本機 agent_id>`（附上實際的 agent_id 與憑證路徑） |
| `certificate_rejected` | 連線期間伺服器重新載入 CA / CRL 後重新驗證，發現憑證已被撤銷、過期或不再受信任 | 客戶端憑證已被撤銷、過期或不再受伺服器信任，請聯絡管理者重新簽發 |

兩者都是 fatal 錯誤，照一般流程退避重連，延遲逐次加長，避免設定錯誤時狂打伺服器。被撤銷的憑證之後重連會在 TLS 握手階段就被拒絕。

## 建置與執行

```bash
# 建置
cargo build            # debug
cargo build --release  # release

# 測試與檢查
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check

# 執行
cargo run
# 或直接執行編譯後的二進位檔
./target/release/pulse
```

命令列參數：

| 參數 | 說明 |
|---|---|
| （無） | 啟動監控代理 |
| `--print-agent-id` | 印出本機 agent_id 後結束（見「啟用 mTLS」） |
| `-h`, `--help` | 顯示使用說明 |

未知參數會印出錯誤與用法，並以 exit code 2 結束。

程式啟動後會：

1. 載入並驗證設定；設定值超出範圍、`log.level` 無效、`ca_cert` 讀不到、客戶端憑證設定不完整或無法載入、`agent.id` 內容不合法或無法建立、客戶端憑證 SAN 與 agent_id 不符時，**立即**印出錯誤並以非 0 結束
2. 清除過期日誌檔（之後每小時一次）
3. 讀取（或第一次產生）`agent_id`
4. 印出一次完整的系統資訊（在背景執行緒進行；收到 Ctrl+C 時不會等它完成）
5. 啟動系統資訊收集背景任務（每 `report.interval` 秒取樣一次）
6. 連線到伺服器、送出 hello、收到 hello_ack 後開始送 heartbeat 與 report

按 Ctrl+C 結束（Windows 上 Ctrl+Break、關閉主控台視窗、系統關機也一樣；其他平台為 SIGINT、SIGTERM）：

- 已連線時，會先對伺服器送出 TLS close_notify（最多等 1 秒），讓 Kairos 看到正常關閉，而不是連線被重設。
- 啟動階段（例如系統資訊收集因 WMI 很慢而卡住）也能立即中斷：Pulse 不會等待仍在背景執行緒上的系統呼叫，收到訊號後約 1 秒內結束。
- 正常結束時 exit code 為 0；收尾期間再送一次訊號會強制結束，exit code 為 130。

若系統資訊收集任務意外結束或 panic，程式會記錄 `ERROR` 並以非 0 結束，交由服務管理員（systemd、Windows 服務等）重新啟動。

## 設定

設定依下列順序疊加，**後者覆蓋前者**：

1. 專案根目錄的 `config.toml`：於**編譯時**以 `include_str!` 嵌入，作為預設值（修改它需重新編譯）
2. 目前工作目錄下的 `pulse.toml`：可不存在；只需寫要覆蓋的欄位，**不需重新編譯**
3. 環境變數 `PULSE__<SECTION>__<KEY>`（雙底線分隔，不分大小寫），例如 `PULSE__SERVER__PORT=9000`、`PULSE__LOG__LEVEL=debug`

設定中的相對路徑（`ca_cert`、`client_cert`、`client_key`、`agent.id_file`）與日誌目錄 `logs/` 一律以**目前工作目錄**解析。

```toml
[server]
address = "127.0.0.1"          # 伺服器位址
port = 8080                    # 伺服器連接埠
domain = "localhost"           # TLS SNI / 憑證名稱驗證用的名稱（DNS 名稱或 IP；須與伺服器憑證 SAN 相符）
accept_invalid_certs = false   # 是否略過伺服器憑證驗證（預設 false）
ca_cert = ""                   # 額外信任的 CA / 自簽憑證 PEM（可含多張）；空字串 = 只用作業系統信任庫
client_cert = ""               # mTLS 客戶端憑證 PEM（可接中繼 CA）；空字串 = 不出示客戶端憑證
client_key = ""                # 對應私鑰 PEM（PKCS#8 / PKCS#1 / SEC1）；須與 client_cert 同時設定

[retry]
max_retries = 5                # 連續失敗達此次數後，重連日誌由 warn 升級為 error（重試不設上限）
initial_delay = 2              # 第一次重連延遲（秒），之後每次加倍
max_delay = 60                 # 重連延遲上限（秒）

[heartbeat]
interval = 1                   # heartbeat 間隔（秒）

[report]
interval = 5                   # 系統資訊回報間隔（秒）

[agent]
id_file = "agent.id"           # 存放 agent_id 的檔案

[log]
level = "info"                 # 日誌級別：trace / debug / info / warn / error（不分大小寫）
retention_days = 14            # 日誌檔保留天數（今天與前 N 天）；0 = 永久保留
```

### 設定值範圍

超出範圍時啟動即報錯結束（例如 `heartbeat.interval 必須介於 1 到 60 之間（目前為 61）`）：

| 欄位 | 範圍 | 說明 |
|---|---|---|
| `heartbeat.interval` | 1～60 秒 | 與伺服器將 `heartbeat_interval_ms` 夾在 60000 一致 |
| `report.interval` | 1～3600 秒 | |
| `retry.initial_delay` | 1～3600 秒 | |
| `retry.max_delay` | 1～3600 秒，且 ≥ `initial_delay` | |
| `log.level` | `trace` / `debug` / `info` / `warn` / `error` | 不分大小寫（`DEBUG`、`Warn` 皆可）；其他值啟動即報錯，不會默默改用 `info` |
| `log.retention_days` | 0～3650 天 | 0 = 永久保留 |
| `agent.id_file` | 非空字串 | |
| `server.domain` | 有效的 DNS 名稱或 IP 位址 | |
| `server.client_cert` / `server.client_key` | 同時設定或同時留空 | 只設定一個時啟動即報錯 |

### ⚠️ 安全性提醒

- `accept_invalid_certs` **預設為 `false`**，Pulse 會驗證伺服器憑證是否受信任、`server.domain` 是否與憑證 SAN 相符。
- 使用自簽憑證或私有 CA 時，請用 `ca_cert` 指定要信任的憑證，**不要**改成 `accept_invalid_certs = true`。
- `accept_invalid_certs = true` 會略過伺服器憑證的信任鏈與名稱驗證，任何人都能冒充伺服器（中間人攻擊），僅限臨時除錯使用；啟用時日誌會輸出警告。握手簽章仍會正確驗證（確保對方持有該憑證的私鑰），但這無法防止中間人使用自己的憑證。若同時設定了 `ca_cert`，也會警告 `ca_cert` 此時形同無效。

## 協定概述

完整規格見 [PROTOCOL.md](PROTOCOL.md)，以下為摘要：

- 一條長 TLS 連線，**NDJSON**（UTF-8，一行一則 JSON，以 `\n` 結尾），單行上限 65536 bytes
- 每則訊息有 `type` 與 `seq`；`seq` 每條連線從 1 開始遞增，重連後重新計算
- 流程：Pulse 送 `hello` → Kairos 回 `hello_ack` → 之後 Pulse 每 `heartbeat.interval` 秒送 `heartbeat`、每 `report.interval` 秒送 `report`，Kairos 對每則訊息回 `ack`
- `report` 的 `ack` 代表該筆資料已寫入伺服器的資料檔；伺服器寫檔失敗時改回非 fatal 的 `storage_unavailable`。Pulse 收到時記 `WARN`（與其他伺服器異常訊息一樣節流），**該筆 report 視為未送達、不重送**，連線保持
- Kairos 也可能回其他 `error`：
  - 收到 `hello_ack` **之前**收到任何 `error`（不論是否 fatal，例如 `bad_request`），Pulse 立即以 `ERROR` 記錄錯誤碼與訊息、結束該連線並退避重連，不會等到 5 秒逾時
  - 註冊之後收到非 fatal 的 `error` 只記 `WARN`；`fatal: true` 時記 `ERROR` 並退避重連
- `hello_ack` 的 `seq` 應為 1（對應 `hello`），不符時只記 `WARN`，仍視為註冊成功
- `hostname` 送出前會去除前後空白、把控制字元替換成 `?`、截到 255 bytes 以內的字元邊界（取不到時為 `unknown`）；`cpu_usage` 送出前若非有限值改為 0，再夾在 0～100

| 項目 | 逾時 |
|---|---|
| TCP 連線 | 5 秒 |
| TLS 握手 | 10 秒 |
| 送出 hello 後等 hello_ack | 5 秒 |
| 每則訊息寫出 | 5 秒 |
| 讀取閒置（沒收到任何伺服器訊息） | `max(10 秒, 2 × min(heartbeat.interval, report.interval))` → 視為斷線並重連 |

以預設值（heartbeat 1 秒、report 5 秒）而言，讀取閒置逾時為 10 秒。下限 10 秒是為了容忍伺服器在忙碌時延遲回 ack（Kairos 寫出佇列滿時最多延遲 5 秒）。

## 回報資料格式

系統資訊以 `report` 訊息送出（一行 JSON，此處為方便閱讀而換行）：

```json
{
  "type": "report",
  "seq": 6,
  "ts": "2026-10-08T03:15:45.210Z",   // Pulse 取樣時間（RFC 3339 UTC，毫秒）
  "cpu_usage": 17.84,                 // 全機 CPU 平均使用率（%，0～100）
  "total_memory": 49152,              // 總記憶體（MB）
  "used_memory": 37034                // 已使用記憶體（MB）
}
```

主機名稱與 `agent_id` 只在連線開頭的 `hello` 送一次，不會重複出現在每筆 report 中。

Report 為 **at-most-once**：斷線期間的取樣、以及伺服器回 `storage_unavailable` 的那一筆，都會直接丟棄，不補送。

## 重連行為

- 任何錯誤（連線失敗、TLS 失敗、逾時、伺服器關閉連線、收到 fatal error、註冊前收到任何 error）都會關閉目前連線，等待後重連。
- 等待時間從 `retry.initial_delay` 秒開始，每次加倍，最多 `retry.max_delay` 秒，並加上 ±20% 隨機抖動，避免大量 agent 同時重連。到達上限後，延遲在 `[0.8 × max_delay, max_delay]` 間均勻分佈（不會全部集中在上限值）。
- **不限重試次數**，程式不會因伺服器暫時不在而結束。
- 連續失敗未達 `retry.max_retries` 次時以 `WARN` 記錄，達到後改用 `ERROR`。
- **連線需在註冊（收到 `hello_ack`）後穩定維持 30 秒以上**，斷線時才會把退避與連續失敗次數歸零。註冊後不到 30 秒就斷（例如兩台主機共用同一份 `agent.id` 而互相被 `superseded` 踢掉）仍算一次失敗，延遲繼續加倍。日誌會註明本次連線維持了多久、退避是否重置，例如：

  ```
  [WARN] 連線中斷（連續第 3 次失敗）：伺服器回報致命錯誤 superseded：…（註冊後僅維持 0.0 秒，未滿 30 秒，退避不重置）；9.2 秒後重連
  [WARN] 連線中斷（連續第 1 次失敗）：…（連線已維持 32 秒，退避已重置）；2.0 秒後重連
  ```

## agent.id

- 第一次啟動時產生一個 UUID v4 並寫入 `agent.id`（路徑由 `agent.id_file` 設定），之後每次啟動讀檔沿用。
- 檔案內容必須是 **UUID 文字格式**：`8-4-4-4-12` 個十六進位字元，共 36 字元，大小寫皆可（例如 `6f1c0e4a-1b2c-4d3e-9f00-112233445566`）。讀檔時會先去除 UTF-8 BOM（Windows 記事本存檔可能加上）與前後空白。
- 內容不合法（含空檔）時，Pulse **不會**自動換一個新 ID（否則伺服器會把它當成另一台主機），而是啟動即以錯誤結束，並提示：要重新產生請刪除該檔案後重新啟動。
- 寫檔不會覆蓋既有檔案，也不會留下寫到一半的檔案：先寫入暫存檔並 `sync`（避免斷電後留下空檔），再以 hard link 發佈；檔案系統不支援 hard link 時（例如 FAT32 / exFAT），改以「僅在不存在時建立」的方式直接寫入。兩個實例同時第一次啟動時，會使用同一個 ID。
- Kairos 以 `agent_id` 識別主機；主機名稱只用於顯示。
- 啟用 mTLS 時，客戶端憑證的 SAN 綁定 `agent_id`；重新產生 `agent.id` 就必須重新簽發憑證。可用 `pulse --print-agent-id` 查看目前的 ID。
- **不要把同一份 `agent.id` 複製到多台主機**（例如複製 VM 映像檔時）：兩台會互相把對方踢下線（伺服器回 `superseded`），退避會逐次加長。若需要重新產生，刪除 `agent.id` 後重新啟動即可。
- `agent.id` 已列入 `.gitignore`。

## 專案結構

```
src/
├── main.rs            進入點：載入設定、初始化日誌、建立 TLS 設定、啟動並監看背景任務
├── cli.rs             命令列參數解析（--print-agent-id、--help）
├── config_manager.rs  設定管理（嵌入預設 → pulse.toml → PULSE__ 環境變數）與範圍驗證
├── tls.rs             rustls 設定：信任庫、客戶端憑證、SAN 檢查、TLS 錯誤提示
├── protocol.rs        協定訊息定義（ClientMsg / ServerMsg）
├── network.rs         長 TLS 連線：hello / heartbeat / report / 逾時 / 重連
├── backoff.rs         指數退避與抖動
├── log_files.rs       日誌按日分檔與保留清除
├── log_throttle.rs    日誌節流（伺服器異常訊息每 10 秒最多記錄一次）
├── agent_id.rs        agent_id 產生、驗證與讀取
├── shutdown.rs        結束訊號監聽（Ctrl+C、Ctrl+Break、主控台關閉、系統關機）
└── system_info.rs     以 sysinfo 收集系統資訊、週期性取樣
testdata/              單元測試用的憑證與私鑰（僅供測試，見 testdata/README.md）
config.toml            預設設定（編譯時嵌入）
rust-toolchain.toml    固定使用 Rust 1.99.0
```

## 日誌

- 同時輸出到終端（stdout）與 `logs/pulse-YYYY-MM-DD.log`（相對於目前工作目錄）
- **按日分檔**：檔名日期為 **UTC**（與 Kairos 相同），UTC 午夜換新檔；每行開頭的時間戳記則是本機時間
- **保留**：保留今天與前 `log.retention_days` 天（預設 14），更早的檔案於啟動時與之後每小時刪除；`0` 表示永久保留。只會刪除檔名**完全符合** `pulse-YYYY-MM-DD.log` 的檔案，其他檔案（例如舊版的 `pulse.log`）一律不碰
- 級別由 `[log] level` 控制，不分大小寫（可用 `PULSE__LOG__LEVEL` 臨時調整）
- 每則 heartbeat 為 `trace`、每則 report 為 `debug`；在 `info` 級別下只會看到連線 / 註冊 / 斷線等事件
- **伺服器異常訊息節流**：伺服器送來無法解析的行、註冊後重複的 `hello_ack`、`storage_unavailable`、其他非致命 `error`、不合預期的 `ack` 這幾類訊息，各自每 10 秒最多記錄一則，期間被略過的數量會在下一則一併註明，例如：

  ```
  [WARN] 已註冊後又收到 hello_ack（seq=1），忽略（上次記錄後的 10 秒內另有 5737 則相同訊息被略過）
  ```

  避免故障或惡意的伺服器在短時間內灌爆日誌檔。會結束連線的錯誤（fatal error、註冊前的 error）不節流，其頻率由重連退避限制。

## 目前限制

- 只回報 CPU 使用率與記憶體；磁碟、網路、溫度僅在啟動時印出到日誌，不會送給伺服器。
- 斷線期間或伺服器無法保存（`storage_unavailable`）的 report 不會暫存補送（at-most-once，協定設計如此）。
- 系統資訊收集任務異常時直接結束程式（不在程式內自動重啟），需搭配服務管理員。

## 授權

尚未指定。
