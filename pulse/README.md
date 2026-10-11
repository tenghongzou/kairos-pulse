# Pulse

輕量級的**地端系統監控代理程式（on-premise monitoring agent）**，以 Rust 撰寫。

Pulse 在本機收集系統資訊（CPU、記憶體），透過**一條長期維持的 TLS 加密連線**向中央伺服器 Kairos 註冊、定期送出心跳（heartbeat）與系統資訊回報（report）。連線中斷時會自動以指數退避無限重連。

## 功能特色

- 🖥️ 收集系統資訊：CPU 使用率、記憶體（啟動時另印出一次系統資訊：磁碟、網路、元件溫度；`info` 等級略過虛擬檔案系統與虛擬網路介面）
- 🔐 單一長 TLS 連線（rustls），NDJSON 協定（一行一則 JSON），預設**驗證伺服器憑證**（作業系統信任庫 + 自訂 CA；可設定為只信任自訂 CA）
- 🪪 每台主機有固定的 `agent_id`（UUID v4，存於 `agent.id`），換主機名稱也不影響識別
- 💓 每秒送出 heartbeat，伺服器據此判斷上線 / 離線
- 🔁 斷線自動重連：指數退避 + ±20% 抖動、有上限、不限次數
- 🔑 可選 mTLS 客戶端憑證：以憑證 SAN 綁定 `agent_id`，防止冒用；支援中繼 CA 鏈與 RSA / ECDSA / Ed25519 私鑰
- ⚙️ 設定三層疊加：嵌入預設值 → `pulse.toml` → `PULSE__*` 環境變數
- 📝 日誌同時輸出到終端與 `logs/pulse-YYYY-MM-DD.log`（按日分檔、自動清除過期檔）

## 系統需求

- [Rust](https://www.rust-lang.org/) 1.99（edition 2024）；monorepo 根目錄的 `rust-toolchain.toml` 會自動選用 1.99.0
- 搭配的伺服器：Kairos（同一個 monorepo 中的 `kairos/`）
- 不需要 OpenSSL 或其他 C 函式庫：TLS 使用 rustls（ring 加密後端）

## 升級注意事項（與 Kairos 混用版本）

Pulse 與 Kairos 必須搭配升級。新舊版本混用時可能出現下列問題：

| 組合 | 風險 |
|---|---|
| 舊版 Pulse（`44dcee5` 之前，即 `081f859` 及更早）連新版 Kairos | 舊版只把主機名稱中的控制字元替換成 `?`，`hostname` 含格式字元（Cf）時被以 `bad_request` 拒絕，**無法註冊**；舊版的讀取閒置固定為 `max(10 秒, 2 × min(heartbeat, report))`，心跳間隔較大時（例如 5 秒）小於「心跳間隔 + 伺服器最壞回應延遲 8 秒」，伺服器寫檔緩慢時**可能誤斷線** |
| 第一輪修正版 Pulse（`44dcee5`）連新版 Kairos | 新版 Kairos 另外禁止 U+034F、Hangul filler、變體選擇字元與私用區（見「協定概述」），`44dcee5` 不會替換這些字元；主機名稱含這些字元時**無法註冊** |
| 新版 Pulse 連舊版 Kairos（`572b955` 之前，`timeouts.write_secs` 沒有 6 秒上限） | Kairos 的 `timeouts.write_secs` 大於約 8 秒時，伺服器寫檔緩慢期間的回應延遲可能超過讀取閒置中的 10 秒餘裕，Pulse **誤判斷線**並重連 |

**部署順序**：先升級全部 Pulse，再升級 Kairos；或兩邊同時升級。新版 Pulse 送出前會替換所有新版 Kairos 禁止的字元，讀取閒置也只依心跳間隔計算，連舊版 Kairos（`timeouts.write_secs` 維持預設 5 秒時）不受影響。

## 從發佈包安裝

地端正式部署請使用預編譯的發佈包 `pulse-<commit>-linux-x86_64.tar.gz`（x86_64、musl 靜態連結，不依賴系統函式庫），不需要安裝 Rust。發佈包內含執行檔、`pulse.example.toml`（內建預設值的參考，與 repo 的 `config.toml` 相同）、`pulse.service`（系統層級 systemd unit 範本）、`INSTALL.md`、本文件與 `PROTOCOL.md`。

完整步驟見 [`packaging/INSTALL.md`](packaging/INSTALL.md)（發佈包根目錄的 `INSTALL.md`），概要如下。所有步驟請在**同一個 root shell**（`sudo -i`）中依序執行，不能只在每個指令前加 `sudo`：`/opt/pulse` 的權限是 750，一般帳號無法 `cd` 進去，而 `agent.id`、私鑰與 CSR 都寫到目前目錄，`AGENT_ID` 變數也要在步驟間延續。

1. `sha256sum -c SHA256SUMS` 驗證後解壓（`tar xzf … --strip-components=1`），`./pulse --version` 確認版本與 commit。
2. 建立系統使用者 `pulse` 與 `/opt/pulse`、`/opt/pulse/logs`，安裝執行檔。
3. `cd /opt/pulse` 後執行 `./pulse --print-agent-id` 產生 `agent.id`；在同一目錄產生私鑰與 CSR，把 CSR（root 600，需以 root 複製出來）交給 Kairos 管理者以 `openssl ca` 簽發（見「啟用 mTLS」）。
4. 建立只含覆蓋欄位的 `pulse.toml`，放好 `ca.pem`、`client.pem`、`client.key` 並設定權限。
5. 安裝 `pulse.service`（`WorkingDirectory=/opt/pulse`、`Restart=always`、`KillSignal=SIGINT`、`StandardOutput=null`：日誌只保留 `logs/` 一份，stderr 仍進 journal），`systemctl enable --now pulse`。
6. 確認日誌出現「已向伺服器註冊」，以及 Kairos 的 `agents.json` 中此 agent 為 `online`。

- **監控**：憑證被撤銷、`auth_failed`、伺服器憑證不受信任或名稱不符等需要人工處理的錯誤，Pulse 會持續重連而不結束，服務仍為 `active (running)`。請看 `systemctl status pulse` 的 `Status:` 行（例如「已註冊：127.0.0.1:18443」「無法註冊：certificate_rejected（…）」「無法連線：伺服器憑證名稱不符（…）」「重新連線中」；需要 unit 的 `NotifyAccess=main`，舊版 unit 沒有這行時不會顯示）、`journalctl -u pulse -p err`、`logs/` 或 Kairos 的 `agents.json`，見 INSTALL.md 第 8.1 節與下方「日誌」。
- **多實例**：同一台主機跑多個 Pulse 時，每個實例使用自己的 `/opt/pulse-N`（`agent.id`、憑證、`pulse.toml`、`logs/` 各自一份），執行檔共用 `/opt/pulse/pulse`；unit 只改 `WorkingDirectory` 與 `ReadWritePaths` 兩行。完整指令見 INSTALL.md 第 9 節。
- **移除**：有多實例時先停用並移除所有 `pulse-N.service`，`daemon-reload`、`reset-failed` 後，最後才刪除 `/opt/pulse` 與使用者 `pulse`（見 INSTALL.md 第 12 節）。

升級時先升級全部 Pulse，再升級 Kairos（見上一節）。發佈包由 [`packaging/build-release.sh`](packaging/build-release.sh) 產生：以 docker 的 `rust:1.99-alpine` 建置 musl 靜態執行檔，輸出到 `dist/`（含 `SHA256SUMS`）；需要 Docker，Windows 請在 Git Bash 執行 `sh packaging/build-release.sh`。

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
[INFO] 已向伺服器註冊（agent_id=…，伺服器離線判定 5000 ms；TLS 加密連線 127.0.0.1:8080，domain=localhost，TLSv1_3，已驗證伺服器憑證）
```

判斷連線成功以這一行為準。TLS 握手完成的「已建立 TLS 加密連線（…）」只在 `debug` 等級記錄：TLS 1.3 下伺服器在握手**之後**才驗證客戶端憑證，握手完成不代表會被接受（例如憑證已撤銷時，接著就會被拒）。

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
- 建議同時設定 `server.use_system_roots = false`：Pulse 只信任 `ca_cert`，不再信任作業系統信任庫，避免任何公開 CA 簽發的同名憑證也被接受。
- 伺服器憑證到期時只要用 CA 重簽，Pulse 端不必改設定。
- `ca.key` 是整個信任鏈的根，務必離線保存。
- 由公開 CA 簽發的伺服器憑證不需要設定 `ca_cert`：Pulse 預設（`use_system_roots = true`）會自動信任作業系統的信任庫。

## 啟用 mTLS（客戶端憑證）

Kairos 設定了客戶端 CA（`auth.client_ca`）時會要求雙向 TLS：每台 Pulse 必須出示由該 CA 簽發、且 **SAN 為 `DNS:<自己的 agent_id>`**、EKU 含 `clientAuth` 的客戶端憑證，否則無法連線。Kairos 預設要求憑證只能有這一個 DNS SAN。這樣只知道某台主機 `agent_id` 的人，無法冒用它，也無法把它踢下線。詳細規則見 [PROTOCOL.md](PROTOCOL.md) 的「身分驗證（mTLS）」。

### 1. 在 Pulse 主機上取得 agent_id

```bash
pulse --print-agent-id
# 6f1c0e4a-1b2c-4d3e-9f00-112233445566
```

- stdout 只有 ID 一行（一律為小寫），方便腳本使用；不連線、不寫日誌。
- `agent.id` 不存在時會先產生（提示訊息寫在 stderr）；既有檔案權限過寬的警告（見「agent.id」）也寫在 stderr；內容不合法時以非 0 結束。
- 請在 Pulse 實際執行的工作目錄下執行（或用相同的 `agent.id_file` 設定），才會讀到同一份 `agent.id`。

### 2. 由管理者簽發客戶端憑證

代理 CA 一開始就要用 `openssl ca` 的資料庫管理，之後才能**撤銷**單一代理的憑證（以 `openssl x509 -req` 簽發的憑證沒有簽發紀錄，要撤銷時只能整套重建）。代理 CA 的建立（`agent-ca/ca.cnf`）、CRL 的產生與撤銷流程以 Kairos README 的「啟用 mTLS」與「撤銷客戶端憑證（CRL）」為準，從發佈包部署時見 Kairos 的 `INSTALL.md`。以下為與 Pulse 相關的部分（`<id>` 換成上一步的 agent_id）：

```bash
# Pulse 主機：產生私鑰與 CSR（私鑰不離開這台主機；RSA 或 EC 皆可，
# 例如 EC P-256 改用：-newkey ec -pkeyopt ec_paramgen_curve:P-256）
MSYS_NO_PATHCONV=1 openssl req -new -newkey rsa:2048 -nodes \
  -keyout client.key -out client.csr -subj "/CN=<id>" \
  -addext "subjectAltName=DNS:<id>" -addext "extendedKeyUsage=clientAuth"

# 管理者：以代理 CA 的 openssl ca 資料庫簽發（agent-ca/ca.cnf 依 Kairos README 建立；
# 在 agent-ca/ 的上層目錄執行，ca.cnf 中的路徑是相對路徑）
MSYS_NO_PATHCONV=1 openssl ca -batch -config agent-ca/ca.cnf -extensions client_ext \
  -in client.csr -out client.pem -notext

# 確認 SAN 只有 DNS:<id>、EKU 為 clientAuth
openssl x509 -in client.pem -noout -ext subjectAltName,extendedKeyUsage
```

只把 `client.csr` 交給管理者，取回 `client.pem`（以及伺服器 CA 的 `ca.pem`）；`client-ca.pem` 與 CRL 由管理者設定在 Kairos（`auth.client_ca`、`auth.crl`）。要撤銷某台代理時，管理者以 `openssl ca -revoke` 撤銷並重新產生 CRL，Kairos 熱重載後該代理會收到 `certificate_rejected` 並被斷線。

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
- 私鑰可為 PKCS#8（`BEGIN PRIVATE KEY`）、PKCS#1（`BEGIN RSA PRIVATE KEY`）或 SEC1（`BEGIN EC PRIVATE KEY`）；演算法支援 RSA、ECDSA P-256 / P-384、Ed25519。不支援有密碼的私鑰：偵測到加密私鑰（`BEGIN ENCRYPTED PRIVATE KEY`，或含 `Proc-Type: 4,ENCRYPTED` 標頭）時啟動即報錯，並提示以 `openssl pkey -in <加密私鑰> -out <新檔案>` 解密後使用（解密後的檔案請限制只有執行帳號可讀）。
- 啟動時就會檢查，任何一項不符都立即以非 0 結束並說明原因：
  - 檔案讀不到、找不到憑證或私鑰、格式錯誤、私鑰有加密
  - **私鑰與憑證不成對**（私鑰的公鑰與憑證不符）
  - **憑證的 SAN 不含 `DNS:<本機 agent_id>`**（伺服器一定會以 `auth_failed` 拒絕，所以不必連線就先報錯）
- 憑證有效期限在啟動時與之後**每 24 小時**檢查一次，都不阻止啟動或連線：
  - 憑證鏈中有已過期的憑證時記 `ERROR`，並提醒檢查本機時鐘；Pulse 仍會繼續嘗試連線，以伺服器的判定為準（本機時鐘超前時，憑證可能其實仍有效）。
  - 有憑證在 30 天內到期（日誌顯示「剩餘不超過 N 天」，天數無條件進位）、或尚未生效（可能是本機時鐘不正確）時，記 `WARN`。
- 客戶端憑證含多個 DNS SAN 時記 `WARN` 但照常啟動：Kairos 預設會以 `auth_failed` 拒絕，除非伺服器設定了 `auth.allow_multiple_dns_sans = true`。
- 成功載入時日誌會出現 `已載入客戶端憑證：<路徑>（憑證鏈 N 張；私鑰：<路徑>）`。
- 私鑰檔案的權限過寬（其他帳號可讀取或修改）時記 `WARN`，不阻止啟動；規則見「敏感檔案的權限檢查」。

**agent.id 與憑證綁定**：重新產生 `agent.id`（刪除檔案、或複製到新主機後重產）就會得到新的 agent_id，必須重新簽發憑證；否則 Pulse 啟動時就會因 SAN 不符而報錯。

### 連線失敗時的提示

TLS 失敗時，Pulse 會在重連日誌中附上具體原因，例如：

| 情況 | 日誌提示 |
|---|---|
| 伺服器要求客戶端憑證但未設定 | 伺服器要求客戶端憑證（mTLS），但未設定 server.client_cert / server.client_key |
| 伺服器不信任此客戶端憑證（簽發 CA 不對、缺中繼 CA、EKU 不含 clientAuth） | 伺服器不信任此客戶端憑證…／伺服器可能不接受此客戶端憑證… |
| 伺服器表示客戶端憑證已撤銷（TLS alert `certificate_revoked`） | 伺服器表示此客戶端憑證已被撤銷，請聯絡管理者重新簽發 |
| 伺服器表示客戶端憑證已過期（TLS alert `certificate_expired`） | 伺服器表示此客戶端憑證已過期，請聯絡管理者重新簽發 |
| 伺服器憑證的簽發者不受信任 | 伺服器憑證不受信任…請以 server.ca_cert 指定 |
| 伺服器憑證名稱與 `server.domain` 不符 | 伺服器憑證的名稱與 server.domain「…」不符 |
| 伺服器憑證被標記為 CA | 伺服器憑證被標記為 CA（basicConstraints CA:TRUE）…加上 basicConstraints=critical,CA:FALSE |
| TLS 握手完成前連線就被關閉（例如超過 Kairos 的每 IP 連線上限 `server.max_connections_per_ip`，多台代理經同一個 NAT 連入時常見；或超過新連線速率、伺服器握手中的連線數上限） | 伺服器在 TLS 握手完成前就關閉了連線：可能超過伺服器的每 IP 連線上限（同一 IP 位址的連線過多）、新連線速率過快（同一 IP，IPv6 為同一 /64）或伺服器握手中的連線過多，或伺服器正在重新啟動、連到的連接埠不是 TLS 服務 |

TLS 1.3 下伺服器在握手**之後**才驗證客戶端憑證，拒絕時送出的 alert 可能因連線被立即重設而收不到。Pulse 會記錄伺服器在握手時是否送出了客戶端憑證要求（CertificateRequest），因此即使沒收到 alert 也能指出是客戶端憑證的問題。

伺服器以錯誤碼區分兩種身分驗證失敗，Pulse 依錯誤碼給出提示：

| 錯誤碼 | 情境 | 日誌提示（`ERROR`） |
|---|---|---|
| `auth_failed` | hello 的 `agent_id` 與客戶端憑證的 SAN 不符 | 客戶端憑證的 SAN 必須是 `DNS:<本機 agent_id>`，且伺服器預設只接受恰好一個 DNS SAN（附上實際的 agent_id 與憑證路徑） |
| `auth_failed`（message 為 `client certificate must contain exactly one DNS SAN`） | 客戶端憑證含多個 DNS SAN | 客戶端憑證含有多個 DNS SAN，請以單一 SAN `DNS:<本機 agent_id>` 重新簽發（或由管理者設定 `auth.allow_multiple_dns_sans = true`） |
| `certificate_rejected` | 伺服器在 hello 時、或連線期間重新載入 CA / CRL 後重新驗證，發現憑證已被撤銷、過期或不再受信任 | 客戶端憑證已被撤銷、過期或不再受伺服器信任，請聯絡管理者重新簽發 |
| `certificate_rejected`（message 為 `revocation status unavailable`） | 伺服器的 CRL 已過期、或缺少某一層 CA 的 CRL，無法判定撤銷狀態 | 伺服器的 CRL 已過期或無法判定此憑證的撤銷狀態，屬伺服器端問題（不是本機憑證的問題，不需要重新簽發），請聯絡伺服器管理者更新 CRL |

另外，Kairos 狀態表已滿時會以 fatal 的 `bad_request`（message 為 `server agent registry is full`）拒絕新的 agent_id，Pulse 會提示「伺服器狀態表已滿，無法登記新的 agent，請管理者調高 limits.max_agents」。

這些都是 fatal 錯誤，照一般流程退避重連，延遲逐次加長，避免設定錯誤時狂打伺服器。被撤銷的憑證之後重連會在 TLS 握手階段就被拒絕。這類錯誤每次重連都會重複發生，因此同一錯誤碼的 `ERROR` 每 10 分鐘最多記一則，期間其他次數改記 `WARN`，下一則 `ERROR` 會註明期間改記 `WARN` 的次數（見「重連行為」）。

Pulse 不會因這些錯誤結束，以 systemd 執行時服務仍為 `active (running)`，只看服務是否在執行的監控會漏掉。以 `ERROR` 記錄的這類錯誤會同時寫一行到 stderr（進 journal，可用 `journalctl -u pulse -p err` 篩選），並以 sd_notify 更新 `systemctl status` 的 `Status:` 文字，例如「無法註冊：certificate_rejected（…）」「無法註冊：TLS alert CertificateRevoked（…）」「無法註冊：auth_failed（…）」，恢復後為「已註冊：<位址>:<連接埠>」（見「日誌」與 `packaging/INSTALL.md` 第 8.1 節）。

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
../target/release/pulse   # workspace 的 target/ 在 monorepo 根目錄
```

測試都寫在各模組內（`#[cfg(test)]`），包含以 `testdata/` 的測試憑證進行的實際 TLS 握手測試，目前共 158 個。

> **`testdata/` 中的私鑰（`*.key`）只供測試使用，已公開在 repo 中，絕對不可用於任何部署環境。** 正式環境的憑證與私鑰請依「私有 CA」與「啟用 mTLS」重新產生。

建議在 CI 加上 `cargo audit`（需先 `cargo install cargo-audit`），檢查依賴套件是否有已公告的安全漏洞。

命令列參數：

| 參數 | 說明 |
|---|---|
| （無） | 啟動監控代理 |
| `--print-agent-id` | 印出本機 agent_id 後結束（見「啟用 mTLS」） |
| `-V`, `--version` | 印出 `pulse <版本> (<commit>)` 後結束（不讀取設定，設定有誤時也能確認版本） |
| `-h`, `--help` | 顯示使用說明 |

未知參數會印出錯誤與用法，並以 exit code 2 結束。

`--version` 的 commit 在建置時決定：環境變數 `BUILD_COMMIT`（`packaging/build-release.sh` 會傳入）→ `git rev-parse --short HEAD` → 都取不到時為 `unknown`。啟動日誌的第一行也會印出同樣的版本字串；送給伺服器的 hello 中的 `agent_version` 仍只有版本號（例如 `0.1.0`）。

程式啟動後會：

1. 載入並驗證設定；設定值超出範圍（含 `server.port` 為 0、`server.address` 或 `server.domain` 空白）、`log.level` 無效、`use_system_roots = false` 卻未設定 `ca_cert`（`accept_invalid_certs = true` 時例外）、`ca_cert` 讀不到、客戶端憑證設定不完整或無法載入（含私鑰加密）、`agent.id` 內容不合法（含超過 1024 bytes、非 UTF-8）或無法建立、客戶端憑證 SAN 與 agent_id 不符時，**立即**印出錯誤並以非 0 結束（客戶端憑證已過期**不會**阻止啟動，見「啟用 mTLS」）
2. 清除過期日誌檔（清除前檢查系統日期是否合理，見「日誌」；之後每小時一次），並以 `INFO` 記錄工作目錄、`pulse.toml` 的完整路徑與是否讀到（工作目錄是 `System32`／`SysWOW64` 時另記 `WARN`，見「設定」）
3. 讀取（或第一次產生）`agent_id`
4. 印出一次系統資訊（在背景執行緒進行；收到 Ctrl+C 時不會等它完成）。`info` 等級下 CPU 只有一行摘要（例如 `CPU: 24 個核心，平均使用率 37.38%（各核心使用率見 debug 等級）`），磁碟略過虛擬檔案系統與容量為 0 的掛載點，以及同一磁碟的重複掛載點（「磁碟名稱＋檔案系統＋總容量」相同者只列一次，保留路徑最短的掛載點，例如 systemd 沙箱的 `ReadWritePaths=`、`PrivateTmp=` bind mount 或 btrfs subvolume；磁碟名稱為空時不合併），網路略過 `lo` 與虛擬介面（`veth*`、`docker*`、`br-*` 等），略過的數量各以一行統計；完整清單需 `log.level = "debug"`。磁碟容量以十進位單位顯示：1 GB 以上為 GB（小數 1 位，例如 `39.4 GB`），未滿 1 GB 為整數 MB（例如 `535 MB`）。取不到的系統名稱、核心版本、主機名稱等顯示「未知」；沒有溫度感測器（例如 VM）時印「未偵測到溫度感測器」
5. 啟動系統資訊收集背景任務（每 `report.interval` 秒取樣一次）
6. 連線到伺服器、送出 hello、收到 hello_ack 後開始送 heartbeat 與 report

按 Ctrl+C 結束（Windows 上 Ctrl+Break、關閉主控台視窗、系統關機也一樣；其他平台為 SIGINT、SIGTERM）：

- 已連線時，會**盡力**先對伺服器送出 TLS close_notify（最多等 1 秒），讓 Kairos 看到正常關閉，而不是連線被重設。Windows 關閉主控台、登出或關機時只給行程有限的時間（一般互動式程式也不一定收得到關機事件），Unix 上的 SIGKILL 也無法攔截，這些情況下 close_notify 不一定送得出去。
- 若收到結束要求時有一則訊息只送出一部分，Pulse **不送** close_notify、直接關閉連線，讓伺服器把它當成未正常關閉的連線並丟棄不完整的行，而不是把半行當成最後一則訊息處理。
- 啟動階段（例如系統資訊收集因 WMI 很慢而卡住、建立 TLS 設定時讀檔很慢）也能立即中斷：Pulse 不會等待仍在背景執行緒上的系統呼叫，收到訊號後約 1 秒內結束。
- 正常結束時 exit code 為 0；收尾期間再送一次訊號會強制結束，exit code 為 130。

若系統資訊收集任務意外結束或 panic，程式會記錄 `ERROR` 並以非 0 結束，交由服務管理員（systemd、Windows 服務等）重新啟動。

## 設定

設定依下列順序疊加，**後者覆蓋前者**：

1. 專案根目錄（monorepo 的 `pulse/`）的 `config.toml`：於**編譯時**以 `include_str!` 嵌入，作為預設值（修改它需重新編譯）；發佈包中的 `pulse.example.toml` 就是這份檔案，只供參考，程式不會讀取
2. 目前工作目錄下的 `pulse.toml`：可不存在；只需寫要覆蓋的欄位，**不需重新編譯**
3. 環境變數 `PULSE__<SECTION>__<KEY>`（雙底線分隔，不分大小寫），例如 `PULSE__SERVER__PORT=9000`、`PULSE__LOG__LEVEL=debug`

設定中的相對路徑（`ca_cert`、`client_cert`、`client_key`、`agent.id_file`）、`pulse.toml` 與日誌目錄 `logs/` 一律以**目前工作目錄**解析。啟動時會以 `INFO` 記錄工作目錄與 `pulse.toml` 的完整路徑、是否讀到，例如：

```
[INFO] 工作目錄：/opt/pulse（設定中的相對路徑都以此為準）；設定檔：已讀取 /opt/pulse/pulse.toml
```

> **以 Windows 服務執行時**，預設工作目錄是 `C:\Windows\System32`：`pulse.toml` 必須放在那裡才會被讀取，`agent.id` 與 `logs/` 也會建立在那裡。Pulse 偵測到工作目錄是 `System32` 或 `SysWOW64` 時會記 `WARN`。請在服務設定中把工作目錄指定為 Pulse 所在的目錄（例如以服務包裝工具設定啟動目錄）。

環境變數的值一律以字串讀入：數值與布林欄位在載入時才轉成對應型別，路徑、`domain`、`address` 等字串欄位保持原樣（例如 `PULSE__AGENT__ID_FILE=0001` 就是 `0001`，不會被改寫成 `1`）。

```toml
[server]
address = "127.0.0.1"          # 伺服器位址
port = 8080                    # 伺服器連接埠
domain = "localhost"           # TLS SNI / 憑證名稱驗證用的名稱（DNS 名稱或 IP；須與伺服器憑證 SAN 相符）
accept_invalid_certs = false   # 是否略過伺服器憑證驗證（預設 false）
ca_cert = ""                   # 額外信任的 CA / 自簽憑證 PEM（可含多張）；空字串 = 只用作業系統信任庫
use_system_roots = true        # 是否信任作業系統信任庫；false 時只信任 ca_cert（此時 ca_cert 必填，accept_invalid_certs = true 時例外）
client_cert = ""               # mTLS 客戶端憑證 PEM（可接中繼 CA）；空字串 = 不出示客戶端憑證
client_key = ""                # 對應私鑰 PEM（PKCS#8 / PKCS#1 / SEC1，不可加密）；須與 client_cert 同時設定

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

`server.use_system_roots` 也可用環境變數 `PULSE__SERVER__USE_SYSTEM_ROOTS=false` 設定。

### 設定值範圍

超出範圍時啟動即報錯結束（例如 `heartbeat.interval 必須介於 1 到 60 之間（目前為 61）`）：

| 欄位 | 範圍 | 說明 |
|---|---|---|
| `server.address` | 非空白字串 | 伺服器的 IP 或主機名稱 |
| `server.port` | 1～65535 | |
| `heartbeat.interval` | 1～60 秒 | 與伺服器將 `heartbeat_interval_ms` 夾在 60000 一致 |
| `report.interval` | 1～3600 秒 | |
| `retry.initial_delay` | 1～3600 秒 | |
| `retry.max_delay` | 1～3600 秒，且 ≥ `initial_delay` | 唯一例外：連續被 `superseded` 時延遲至少 30 秒（見「重連行為」） |
| `log.level` | `trace` / `debug` / `info` / `warn` / `error` | 不分大小寫（`DEBUG`、`Warn` 皆可）；其他值啟動即報錯，不會默默改用 `info` |
| `log.retention_days` | 0～3650 天 | 0 = 永久保留 |
| `agent.id_file` | 非空字串 | |
| `server.domain` | 有效的 DNS 名稱或 IP 位址，不可空白 | |
| `server.use_system_roots` | `true` / `false` | `false` 時 `server.ca_cert` 必填，否則啟動即報錯（`accept_invalid_certs = true` 時不使用信任庫，不檢查此項） |
| `server.client_cert` / `server.client_key` | 同時設定或同時留空 | 只設定一個時啟動即報錯 |

### ⚠️ 安全性提醒

- `accept_invalid_certs` **預設為 `false`**，Pulse 會驗證伺服器憑證是否受信任、`server.domain` 是否與憑證 SAN 相符。
- 使用自簽憑證或私有 CA 時，請用 `ca_cert` 指定要信任的憑證，**不要**改成 `accept_invalid_certs = true`；並建議設定 `use_system_roots = false`，只信任 `ca_cert`。
- `accept_invalid_certs = true` 會略過伺服器憑證的信任鏈與名稱驗證，任何人都能冒充伺服器（中間人攻擊），僅限臨時除錯使用。啟用時啟動階段會以 `ERROR` 記錄警告（`log.level` 最高只能設為 `error`，因此一定看得到），每次連線時，連線當下（「已建立 TLS 加密連線」）與註冊成功時（「已向伺服器註冊」）各記一行 `WARN`，並標註「⚠️ 未驗證伺服器憑證」。握手簽章仍會正確驗證（確保對方持有該憑證的私鑰），但這無法防止中間人使用自己的憑證。若同時設定了 `ca_cert`，也會以 `ERROR` 警告 `ca_cert` 此時形同無效。
- 未啟用 mTLS 時，`agent.id` 的內容就是本機在伺服器上的身分憑據，請妥善保護（見「agent.id」）。

## 協定概述

完整規格見 [PROTOCOL.md](PROTOCOL.md)，以下為摘要：

- 一條長 TLS 連線，**NDJSON**（UTF-8，一行一則 JSON，以 `\n` 結尾），單行上限 65536 bytes
- 每則訊息有 `type` 與 `seq`；`seq` 每條連線從 1 開始遞增，重連後重新計算
- 流程：Pulse 送 `hello` → Kairos 回 `hello_ack` → 之後 Pulse 每 `heartbeat.interval` 秒送 `heartbeat`、每 `report.interval` 秒送 `report`，Kairos 對每則訊息回 `ack`
- `report` 的 `ack` 代表該筆資料已寫入伺服器的資料檔；伺服器確定沒有寫入時改回非 fatal 的 `storage_unavailable`（message 為 `report could not be stored`：寫入失敗或達到伺服器的每日檔案大小上限；或 `report rate limit exceeded`：同一來源送得太快）。Pulse 收到時記 `WARN`（節流，見「日誌」），**該筆 report 視為未送達、不重送**，連線保持。伺服器無法確定是否已寫入時會不回應、直接關閉連線，Pulse 視為斷線並重連，該筆同樣不重送
- **同一時間只有一筆 report 在途**：送出 report 後，到收到伺服器對它（或之後的訊息）的回應（`ack` 或帶 `seq` 的 `error`，含 `storage_unavailable`）之前，新的取樣直接略過（記 `WARN`，節流，附這條連線已略過的次數）；斷線時清除。伺服器寫檔緩慢時不會在伺服器端無限積壓
- Kairos 也可能回其他 `error`：
  - 收到 `hello_ack` **之前**收到任何 `error`（不論是否 fatal，例如 `bad_request`），Pulse 立即結束該連線並退避重連，不會等到 5 秒逾時；這次斷線以 `ERROR` 記錄錯誤碼與訊息
  - 註冊之後收到非 fatal 的 `error` 只記 `WARN`（節流）；`fatal: true` 時結束連線並退避重連，以 `ERROR` 記錄（同一錯誤碼每 10 分鐘最多一則 `ERROR`，其餘記 `WARN`）。例外是 `superseded`：單次被取代記 `WARN`，連續被取代才記 `ERROR`（見「重連行為」）
  - `error` 缺少欄位時不會被忽略：缺 `message` 視為空字串、缺 `code` 時日誌顯示「（未提供錯誤碼）」、缺 `fatal` 視為 fatal
- `hello_ack` 的 `seq` 應為 1（對應 `hello`），不符時只記 `WARN`，仍視為註冊成功
- `agent_id` 一律以小寫送出（伺服器也不分大小寫、以小寫為準）
- `hostname` 送出前會去除前後空白、把控制字元（Cc）、格式字元（Cf，例如 U+200B、U+202E、U+FEFF）、U+2028、U+2029，以及顯示為空白或不可見的碼位（U+034F、Hangul filler U+115F／U+1160／U+3164／U+FFA0、變體選擇字元 U+FE00–U+FE0F／U+E0100–U+E01EF、私用區 U+E000–U+F8FF／U+F0000–U+FFFFD／U+100000–U+10FFFD）替換成 `?`（與 Kairos 驗證 hostname 的規則相同，因此不會因主機名稱含這些字元而被拒絕）、截到 255 bytes 以內的字元邊界（取不到時為 `unknown`）；`cpu_usage` 送出前若非有限值改為 0，再夾在 0～100，並四捨五入到小數 2 位

| 項目 | 逾時 |
|---|---|
| TCP 連線 | 5 秒 |
| TLS 握手 | 10 秒 |
| 送出 hello 後等 hello_ack | 5 秒 |
| 每則訊息寫出 | 5 秒 |
| 讀取閒置（沒收到任何**合法**的伺服器訊息；無法解析的行不算） | `max(2 × h, h + 10 秒)`，`h = heartbeat.interval`（與 `report.interval` 無關）→ 視為斷線並重連 |

以預設值（heartbeat 1 秒）而言，讀取閒置逾時為 `max(2, 1 + 10)` = 11 秒；heartbeat 30 秒時為 60 秒。只有能解析的伺服器訊息（`ack`、`hello_ack`、`error`）才會重置閒置計時，持續送垃圾行的伺服器仍會被判定為閒置。逾時在心跳間隔 `h` 上加 10 秒餘裕，是為了容忍伺服器最多 8 秒的回應延遲：Kairos 等待 report 寫入資料檔期間，同一連線的 heartbeat 也要排隊；Kairos 把寫入逾時（`timeouts.write_secs`，預設 5 秒）的上限定為 6 秒，從收到 report 到回應最壞 0.05（訊息速率）+ 0.5（report 速率限制）+ 6 + 1（寫入寬限）= 7.55 秒 ≤ 8 秒。Pulse 每 `h` 秒內一定會送出 heartbeat，因此最久 `h + 8` 秒內一定會收到伺服器訊息，對任何心跳間隔都小於逾時。逾時不看 `report.interval`：report 依賴取樣任務，取樣卡住時不會送出，不能拿來保證伺服器有回應。

## 回報資料格式

系統資訊以 `report` 訊息送出（一行 JSON，此處為方便閱讀而換行）：

```json
{
  "type": "report",
  "seq": 6,
  "ts": "2026-10-08T03:15:45.210Z",   // Pulse 取樣時間（RFC 3339 UTC，毫秒）
  "cpu_usage": 17.84,                 // 全機 CPU 平均使用率（%，0～100，四捨五入到小數 2 位；上次取樣到這次取樣之間的平均）
  "total_memory": 49152,              // 總記憶體（MB）
  "used_memory": 37034                // 已使用記憶體（MB）
}
```

主機名稱與 `agent_id` 只在連線開頭的 `hello` 送一次，不會重複出現在每筆 report 中。

`cpu_usage` 是「上次取樣到這次取樣之間」（約等於 `report.interval`）所有 CPU 的平均使用率：取樣任務沿用同一個 sysinfo `System`，每次取樣只刷新一次，而不是只量約 200 ms 的瞬間值。

Report 為 **at-most-once**：斷線期間的取樣、上一筆 report 尚未收到回應時的取樣、以及伺服器回 `storage_unavailable` 的那一筆，都會直接丟棄，不補送。

## 重連行為

- 任何錯誤（連線失敗、TLS 失敗、逾時、伺服器關閉連線、收到 fatal error、註冊前收到任何 error）都會關閉目前連線，等待後重連。
- 等待時間從 `retry.initial_delay` 秒開始，每次加倍，最多 `retry.max_delay` 秒，並加上 ±20% 隨機抖動，避免大量 agent 同時重連。到達上限後，延遲在 `[0.8 × max_delay, max_delay]` 間均勻分佈（不會全部集中在上限值）。
- **不限重試次數**，程式不會因伺服器暫時不在而結束。
- 每次斷線只記**一則**日誌，內容包含斷線原因、處置提示（若有）、本次連線維持多久與退避是否重置，以及下次重連的等待時間。連續失敗未達 `retry.max_retries` 次時以 `WARN` 記錄，達到後改用 `ERROR`；伺服器送來的 fatal error、註冊前收到的 error 與協定版本不符以 `ERROR` 記錄（`superseded` 例外，見下）。這些錯誤每次重連都會重複，因此依錯誤碼節流：同一錯誤碼每 10 分鐘最多一則 `ERROR`，期間其他次數改記 `WARN`，下一則 `ERROR` 會註明「上次以 ERROR 記錄後，另有 n 次相同原因的斷線已改記 WARN」；錯誤碼改變時立即以 `ERROR` 記錄。
- **連線需在註冊（收到 `hello_ack`）後穩定維持 30 秒以上**，斷線時才會把退避與連續失敗次數歸零。註冊後不到 30 秒就斷仍算一次失敗，延遲繼續加倍。
- **被 `superseded`（同一 `agent_id` 的新連線取代）**：
  - 第一次被取代視同一般斷線：連線已維持 30 秒以上就照常重置退避，記 `WARN`。
  - **連續第 2 次**起，不論連線維持多久都**不重置退避**，且重連延遲**至少 30 秒**（不受 `retry.max_delay` 限制）：兩台主機共用同一份 `agent.id` 時，雙方各自維持 30 秒以上再被對方踢掉，若重置或延遲太短就會永久快速互踢。這時以 `ERROR` 記錄並提示極可能有多台主機共用同一份 `agent.id`（設定了客戶端憑證時另外提示：產生新的 agent_id 後必須以新的 agent_id 重新簽發客戶端憑證，否則 Pulse 會因 SAN 不符而無法啟動）；互踢會持續發生，因此 `ERROR` 每 10 分鐘最多一則，期間其他次數改記 `WARN`，下一則 `ERROR` 會註明「上次以 ERROR 記錄後，另有 n 次「連續被取代」的斷線已改記 WARN」。
  - 因其他原因斷線時，連續被取代的次數歸零。
- 日誌範例：

  ```
  [WARN] 連線中斷（連續第 1 次失敗）：伺服器回報致命錯誤 superseded：superseded by a newer connection（此連線已被同一 agent_id 的新連線取代；若非預期，請確認是否有多台主機共用同一份 agent.id）（連線已維持 45 秒，首次被取代，退避已重置）；2.1 秒後重連
  [ERROR] 連線中斷（連續第 2 次失敗）：伺服器回報致命錯誤 superseded：superseded by a newer connection（已連續 2 次被同一 agent_id（…）的其他連線取代：極可能有多台主機共用同一份 agent.id…）（註冊後維持 0.3 秒，連續被取代，退避不重置）；30.0 秒後重連（連續被取代時延遲至少 30 秒，不受 retry.max_delay 限制）
  [WARN] 連線中斷（連續第 2 次失敗）：伺服器關閉了連線（註冊後僅維持 3.1 秒，未滿 30 秒，退避不重置）；4.4 秒後重連
  [WARN] 連線中斷（連續第 1 次失敗）：…（連線已維持 32 秒，退避已重置）；2.0 秒後重連
  ```

## agent.id

- 第一次啟動時產生一個 UUID v4 並寫入 `agent.id`（路徑由 `agent.id_file` 設定），之後每次啟動讀檔沿用。
- 檔案內容必須是 **UUID 文字格式**：`8-4-4-4-12` 個十六進位字元，共 36 字元，大小寫皆可（例如 `6f1c0e4a-1b2c-4d3e-9f00-112233445566`）。讀檔時會先去除 UTF-8 BOM（Windows 記事本存檔可能加上）與前後空白。
- 識別**不分大小寫**：讀入後一律轉成小寫再使用（hello、日誌、`--print-agent-id`、客戶端憑證 SAN 檢查），與伺服器的正規化一致；檔案本身不會被改寫。
- 檔案超過 **1024 bytes**（例如 `agent.id_file` 誤指到其他大檔案）時不會整個讀進記憶體，而是啟動即報錯；內容不是 UTF-8 文字時也明確報錯。
- 內容不合法（含空檔）時，Pulse **不會**自動換一個新 ID（否則伺服器會把它當成另一台主機），而是啟動即以錯誤結束，並提示：要重新產生請刪除該檔案後重新啟動。
- 寫檔不會覆蓋既有檔案，也不會留下寫到一半的檔案：先寫入暫存檔並 `sync`（避免斷電後留下空檔），再以 hard link 發佈；檔案系統不支援 hard link 時（例如 FAT32 / exFAT），改以「僅在不存在時建立」的方式直接寫入。兩個實例同時第一次啟動時，會使用同一個 ID。
- **未啟用 mTLS 時，`agent_id` 就是本機在伺服器上的身分憑據**：知道它的人可以冒用本機註冊，並把本機踢下線。因此：
  - Unix 上新建的 `agent.id` 權限為 `0600`（僅擁有者可讀寫）。既有檔案的權限過寬時，啟動時記 `WARN`（不會自動修改權限、也不阻止啟動）；規則見下方「敏感檔案的權限檢查」，與 `server.client_key` 相同。
  - Windows 上檔案權限繼承自所在目錄的 ACL；Pulse 啟動時檢查 ACL，Users、Everyone 等一般使用者可讀取或寫入時記 `WARN`。請把 `agent.id`（`agent.id_file`）放在只有執行帳號、SYSTEM 與 Administrators 可存取的目錄。
  - 正式環境建議啟用 mTLS，讓身分改由客戶端憑證與私鑰保護。
- Kairos 以 `agent_id` 識別主機；主機名稱只用於顯示。
- 啟用 mTLS 時，客戶端憑證的 SAN 綁定 `agent_id`；重新產生 `agent.id` 就必須重新簽發憑證。可用 `pulse --print-agent-id` 查看目前的 ID。
- **不要把同一份 `agent.id` 複製到多台主機**（例如複製 VM 映像檔時）：兩台會互相把對方踢下線（伺服器回 `superseded`）；連續被取代時退避不再重置、重連間隔至少 30 秒，日誌以 `ERROR` 提示。若需要重新產生，刪除 `agent.id` 後重新啟動即可；**啟用 mTLS 時**還必須請管理者以新的 agent_id 重新簽發客戶端憑證並更新 `server.client_cert`／`server.client_key`，否則 Pulse 啟動時會因憑證 SAN 不符而報錯結束。
- `agent.id` 已列入 `.gitignore`。

### 敏感檔案的權限檢查

`agent.id`（未啟用 mTLS 時等同身分憑據）與 `server.client_key`（mTLS 私鑰）在啟動時檢查權限；`--print-agent-id` 也會檢查 `agent.id`，警告寫在 stderr。權限過寬時只記 `WARN`，不阻止啟動、不自動修改權限。

- **Unix**（規則與 PostgreSQL 檢查 TLS 私鑰相同）：
  - 其他使用者有任何權限，或群組有寫入、執行權限 → `WARN`。
  - 群組可讀：擁有者是 root 時允許（`root:pulse 640`，服務帳號只能讀不能改，見 `packaging/INSTALL.md`）；擁有者不是 root 時 `WARN`（同群組的其他帳號也能讀）。
  - 擁有者自己的權限不檢查：`pulse:pulse 600`（舊版 INSTALL 的配置）不警告。
- **Windows**：讀取檔案的 DACL，下列群組有讀取或寫入權限的「允許」項目時 `WARN`：Everyone、Authenticated Users、BUILTIN\Users、INTERACTIVE、ANONYMOUS LOGON、本機帳戶（Local account）、Guests、Domain Users、Domain Guests；沒有 DACL（NULL DACL）也 `WARN`。
  - `C:\`、`C:\ProgramData`、`C:\Program Files` 下新建的檔案預設讓 Users 可讀，放在這些位置會收到警告。請放在只有執行帳號、SYSTEM 與 Administrators 可存取的目錄，或以 `icacls "<檔案>" /inheritance:r /grant:r "<執行帳號>:R" "*S-1-5-18:F" "*S-1-5-32-544:F"` 移除繼承並只授權給這些帳號。
  - 限制：只看「允許」項目，不計算「拒絕」項目與實際有效權限（同時有拒絕項目時仍會警告），也不展開群組成員（授權給自訂群組或其他個別帳號不會警告）。讀不到 ACL 時不檢查。

## 專案結構

以下路徑相對於 monorepo 的 `pulse/`；`Cargo.lock` 與 `rust-toolchain.toml` 位於 monorepo 根目錄，建置輸出在根目錄的 `target/`。

```
src/
├── main.rs            進入點：載入設定、初始化日誌、建立 TLS 設定、啟動並監看背景任務
├── cli.rs             命令列參數解析（--print-agent-id、--version、--help）與版本字串
├── config_manager.rs  設定管理（嵌入預設 → pulse.toml → PULSE__ 環境變數）與範圍驗證
├── file_perms.rs      敏感檔案（agent.id、client_key）的權限檢查（Unix 權限位元、Windows ACL）
├── tls.rs             rustls 設定：信任庫、客戶端憑證、SAN 與有效期限檢查、TLS 錯誤提示
├── protocol.rs        協定訊息定義（ClientMsg / ServerMsg）
├── network.rs         長 TLS 連線：hello / heartbeat / report / 逾時 / 重連 / 日誌清理
├── backoff.rs         指數退避與抖動
├── log_files.rs       日誌按日分檔與保留清除（含系統日期異常時暫停清除）
├── log_throttle.rs    日誌節流（同類訊息每個視窗最多記錄一則）
├── agent_id.rs        agent_id 產生、驗證與讀取
├── shutdown.rs        結束訊號監聽（Ctrl+C、Ctrl+Break、主控台關閉、系統關機）
├── system_info.rs     以 sysinfo 收集系統資訊、週期性取樣
└── test_log.rs        測試用：擷取日誌輸出（僅測試建置）
testdata/              測試用的憑證與私鑰（僅供測試、不可部署，見 testdata/README.md）：客戶端 CA / 中繼 CA / 客戶端憑證、
                       TLS 握手測試用的伺服器 CA 與伺服器憑證（server-ca.pem、server.pem、server.key）、
                       多個 DNS SAN 的客戶端憑證（multi-san.pem）
config.toml            預設設定（編譯時嵌入；發佈包中改名為 pulse.example.toml）
build.rs               建置時決定 --version 顯示的 commit（BUILD_COMMIT 或 git）
packaging/             發佈包：build-release.sh（打包腳本）、pulse.service（systemd unit）、INSTALL.md（安裝指南）
```

## 日誌

- 同時輸出到終端（stdout）與 `logs/pulse-YYYY-MM-DD.log`（相對於目前工作目錄）
- **需要人工處理的錯誤另寫一行到 stderr**：伺服器憑證驗證錯誤（不受信任、名稱不符、過期、尚未生效、被撤銷、用途不符、被標記為 CA、其他驗證失敗；第一次發生就記 `ERROR`，原因改變時立即再記）、伺服器送來的致命錯誤與註冊前的錯誤（`certificate_rejected`、`auth_failed`、狀態表已滿等）、協定版本不符、連續被取代、客戶端憑證被伺服器以 TLS alert 拒絕（撤銷、過期等），以及本機客戶端憑證已過期。內容與日誌的 `ERROR` 行相同，節流也相同（同一原因每 10 分鐘最多一行，其餘只在日誌記 `WARN`）；一般連線錯誤（拒絕連線、逾時、連線被重設等）不寫。以 systemd 執行（有 `JOURNAL_STREAM` 環境變數）時行首加上 `<3>`，journal 以 err 等級記錄（`journalctl -u pulse -p err`），訊息中不會出現 `<3>`。stdout 與 stderr 都是終端機（互動執行）時不重複寫。`pulse.service` 設定 `StandardOutput=null`，所以 journal 中只會看到這些行與啟動失敗等 stderr 訊息
- **systemd 狀態文字**：Linux 上有 `NOTIFY_SOCKET` 時以 sd_notify 送出 `STATUS=`，`systemctl status` 顯示為 `Status:` 行：`已註冊：<位址>:<連接埠>`、`重新連線中`（一般連線錯誤）、`無法連線：伺服器憑證<原因>（<處理提示>）`（例如 `無法連線：伺服器憑證不受信任（簽發者不在信任庫中，自簽憑證或私有 CA 請以 server.ca_cert 指定）`、`無法連線：伺服器憑證名稱不符（請把 server.domain 設為伺服器憑證 SAN 中的名稱）`；原因另有已過期、尚未生效、已被撤銷、用途不符、被標記為 CA、驗證失敗）、`無法註冊：<錯誤碼>（<處理提示>）`（錯誤碼如 `certificate_rejected`、`TLS alert CertificateRevoked`、`auth_failed`、`bad_request`、`unsupported_version`、`proto_mismatch`、`superseded`）。只送狀態文字、不送 `READY=1`，unit 維持 `Type=simple`，需設定 `NotifyAccess=main`（發佈包的 `pulse.service` 已設定）；沒有 `NOTIFY_SOCKET` 時（Windows、直接執行）不做任何事
- **按日分檔**：檔名日期為 **UTC**（與 Kairos 相同），UTC 午夜換新檔；每行開頭的時間戳記則是本機時間
- **保留**：保留今天與前 `log.retention_days` 天（預設 14），更早的檔案於啟動時與之後每小時刪除；`0` 表示永久保留。只會刪除檔名**完全符合** `pulse-YYYY-MM-DD.log` 的檔案，其他檔案（例如舊版的 `pulse.log`）一律不碰
- **系統日期異常時暫停清除**（規則與 Kairos 相同；`log.retention_days = 0` 時不檢查）：系統時鐘被誤校到未來時，以「今天」計算的保留期限會把所有日誌檔判定為過期，因此清除前先檢查日期是否合理，「已接受的日期」只能隨實際經過的時間推進：
  - 基準點（可信的日期與當時的時間）只在三種情況設定：啟動時沒有狀態、接受跳躍後的日期、日期回撥後降回已接受的日期。
  - 經過時間在同一次執行內用單調時鐘；跨重新啟動時為「存檔時已經過的時間 + min(max(0, 現在 − 存檔時間), 24 小時)」（每次檢查都存檔）。
  - 日期往前推進時，允許的最晚日期 = 基準日期 + ceil(經過時間 / 1 天) + 1 天；在範圍內照常清除。超過時記 `WARN` 並暫停清除，新的日期持續 24 小時後才接受（並重設基準點）、恢復清除；暫停期間日期比「偵測到跳躍時的日期」再往前超過 2 天時重新起算。
  - 日期回撥（早於已接受的日期）時照常以較早的日期清除（只會刪得更少）；持續 24 小時後把已接受的日期降回目前日期。
  - 跳躍暫停跨重新啟動時以「存檔時已累積的暫停時間」延續，只再加上停機時間（最多 1 小時），停機時把時鐘調快再啟動不會被立即接受；回撥計時延續 min(max(0, 現在 − 起點), 24 小時)，起點在未來時改寫為現在。
  - 狀態存在 `logs/.retention-guard.json`（原子寫入）。沒有狀態檔或內容無效時，以啟動前 `logs/` 中不晚於今天的最新日誌檔日期為基準（基準在初始化日誌、建立今天的日誌檔之前決定）。
  - 副作用：Pulse 停機超過約 2 天後重新啟動時，會記 `WARN` 並暫停清除日誌 24 小時。
  - **請勿刪除 `logs/.retention-guard.json`**：刪掉時若系統時鐘仍停在未來，重新建立的基準可能採用時鐘錯誤期間產生的日誌檔日期而誤刪。搬移或備份日誌目錄時請一併保留。
- 級別由 `[log] level` 控制，不分大小寫（可用 `PULSE__LOG__LEVEL` 臨時調整）
- 每則 heartbeat 為 `trace`、每則 report 為 `debug`；在 `info` 級別下只會看到連線 / 註冊 / 斷線等事件
- **伺服器送來的文字一律清理後才寫入日誌**：`error` 的 `code`、`message`、無法解析的原始行，以及可能夾帶伺服器內容的 TLS／TCP／連線 I/O 錯誤文字（例如憑證名稱不符時列出的伺服器憑證 SAN）中，換行、ESC 等控制字元（Cc）、所有格式字元（Cf，例如零寬空格 U+200B、方向控制 U+202E、BOM U+FEFF）、U+2028 / U+2029，以及 Hangul filler、變體選擇字元、私用區等不可見碼位都會轉成跳脫字串（判定規則與 hostname 正規化、Kairos 的 hello 欄位驗證相同）（例如 `\n`、`\u{001b}`）；反斜線本身轉成兩個反斜線（`\\`），因此原文中的字面 `\n` 與被跳脫的換行可以區分。`code` 最多保留 64 個字元，TLS／I/O 錯誤文字最多 400 個字元，其他文字最多 200 個字元，超過時截斷並註明原始長度。避免惡意或故障的伺服器偽造日誌行、夾帶終端跳脫序列或以超長內容灌爆日誌
- **伺服器異常訊息節流**：伺服器送來無法解析的行、註冊後重複的 `hello_ack`、其他非致命 `error`、不合預期的 `ack`、因上一筆 report 尚未收到回應而略過取樣，這幾類訊息各自每 10 秒最多記錄一則；`storage_unavailable` 每 5 分鐘最多一則（伺服器的每日檔案大小上限滿時，每筆 report 都會收到，會持續到換日）。期間被略過的數量會在下一則一併註明，例如：

  ```
  [WARN] 已註冊後又收到 hello_ack（seq=1），忽略（自上次記錄以來另有 5737 則同類訊息被略過；同類訊息每 10 秒最多記錄一則）
  ```

  避免故障或惡意的伺服器在短時間內灌爆日誌檔。會結束連線的錯誤（fatal error、註冊前的 error）每次斷線只記一則（見「重連行為」），其頻率由重連退避限制；其中需要人工處理的錯誤另依錯誤碼節流 `ERROR`（每 10 分鐘最多一則，其餘記 `WARN`）。

## 目前限制

- 只回報 CPU 使用率與記憶體；磁碟、網路、溫度僅在啟動時印出到日誌，不會送給伺服器。
- 斷線期間、上一筆 report 尚未收到回應時，或伺服器無法保存（`storage_unavailable`）的 report 不會暫存補送（at-most-once，協定設計如此）。
- Windows 服務模式的預設工作目錄是 `System32`，需自行在服務設定中指定工作目錄（見「設定」）。
- 系統資訊收集任務異常時直接結束程式（不在程式內自動重啟），需搭配服務管理員。
- 兩台主機共用同一份 `agent.id` 時，Pulse 只能拉長重連間隔（連續被取代時至少 30 秒）並以 `ERROR` 提示，無法自動解決，雙方仍會持續互踢。
- `hostname` 只供顯示：送出前會把控制字元、格式字元（零寬空格、方向控制等）、U+2028 / U+2029 與不可見碼位（Hangul filler、變體選擇字元、私用區等）替換成 `?`，並截到 255 bytes 以內，伺服器上顯示的名稱可能與實際主機名稱略有不同（識別以 `agent_id` 為準）。
- Windows 上的檔案權限檢查只看 ACL 中授權給一般使用者群組的「允許」項目，不計算「拒絕」項目與實際有效權限，也不展開自訂群組（見「敏感檔案的權限檢查」）。

## 授權

尚未指定。
