# Pulse

輕量級的**地端系統監控代理程式（on-premise monitoring agent）**，以 Rust 撰寫。

Pulse 在本機收集系統資訊（CPU、記憶體），透過**一條長期維持的 TLS 加密連線**向中央伺服器 Kairos 註冊、定期送出心跳（heartbeat）與系統資訊回報（report）。連線中斷時會自動以指數退避無限重連。

## 功能特色

- 🖥️ 收集系統資訊：CPU 使用率、記憶體；Kairos 支援協定 v2 時另外回報 27 個標準指標（磁碟用量與使用率、磁碟 I/O、網路流量／封包／錯誤、負載、可用記憶體、置換空間、行程數、開機時間、溫度；0.8.0 起），Kairos 0.8.0 以上另有行程排行（依 CPU、記憶體）與主機資產（OS、CPU、記憶體、廠牌型號序號、BIOS、磁碟、網卡 MAC／IP；見「回報資料格式」）
- 🔐 單一長 TLS 連線（rustls），NDJSON 協定（一行一則 JSON，v1、v2 自動協商），預設**驗證伺服器憑證**（設定了自訂 CA `ca_cert` 時只信任它，否則使用作業系統信任庫；可明確設定同時信任兩者）
- 🪪 每台主機有固定的 `agent_id`（UUID v4，存於 `agent.id`），換主機名稱也不影響識別
- 💓 每 15 秒（`heartbeat.interval`）送出 heartbeat，伺服器據此判斷上線 / 離線
- 💾 本機緩衝（at-least-once）：取樣先寫進本機磁碟，Kairos 確認後才刪除；斷線、Kairos 維護或寫入結果不明時，重新連上後依序補送（需要 Kairos 0.3.0 以上，見「本機緩衝」）
- 🔁 斷線自動重連：指數退避 + ±20% 抖動、有上限、不限次數；啟動與斷線後第一次重連另加隨機延遲，避免大量 agent 同時連線
- 🔑 可選 mTLS 客戶端憑證：以憑證 SAN 綁定 `agent_id`，防止冒用；支援中繼 CA 鏈與 RSA / ECDSA / Ed25519 私鑰
- ⚙️ 設定三層疊加：嵌入預設值 → `pulse.toml` → `PULSE__*` 環境變數
- 📝 日誌同時輸出到終端與 `logs/pulse-YYYY-MM-DD.log`（按日分檔、自動清除過期檔）；有固定事件的行以「`<event> <代碼>`」開頭（見「日誌」與 `ERROR-CODES.md`）
- 🛰️ 集中設定與遠端升級（0.9.0，搭配 Kairos 0.9.0 以上）：Kairos 可下發白名單中的設定（本機明確寫出的值永遠優先，隱私相關的採集只能更保守），並推送官方簽章的新版 Pulse（Linux tar／deb／rpm；以本機信任錨驗證，替換後健康確認、失敗自動回滾；見「集中設定與遠端升級」）
- 🩺 `pulse status`：讀取執行中的 Pulse 每 10 秒寫出的本機狀態檔，顯示服務、連線、回報（含待補送筆數）、身分、設定與時鐘；`--brief`、`--json` 與結束代碼給監控系統、SCCM 使用（見「本機狀態（pulse status）」）

## 系統需求

- [Rust](https://www.rust-lang.org/) 1.99（edition 2024）；monorepo 根目錄的 `rust-toolchain.toml` 會自動選用 1.99.0
- 搭配的伺服器：Kairos（同一個 monorepo 中的 `kairos/`）
- 不需要 OpenSSL 或其他 C 函式庫：TLS 使用 rustls（ring 加密後端）

## 升級注意事項（與 Kairos 混用版本）

**協定 v2 起不必同時升級**：新版 Pulse 在 hello 宣告支援協定 1～2，依 Kairos 的 `hello_ack` 決定格式（PROTOCOL.md「版本協商」）：

| 組合 | 協商結果 |
|---|---|
| 新版 Pulse 連舊版 Kairos | v1：舊版 Kairos 只檢查 `proto == 1` 並忽略 v2 欄位；Pulse 送 v1 的三欄位 report，註冊日誌為「協定 v1」 |
| 新版 Pulse 連新版 Kairos | v2：report 改為通用指標（CPU、記憶體、磁碟、網路、溫度），另送主機指紋等主機資訊；註冊日誌為「協定 v2」 |
| 舊版 Pulse 連新版 Kairos | v1：與舊版相同 |
| Pulse 0.3.0 以上連 Kairos 0.3.0 以上 | v2＋`durable`：report 為 at-least-once（見「本機緩衝」），Kairos 的上線日誌註明「durable」。0.4 的協定與 0.3 相同（`pulse status` 只讀本機狀態檔），Pulse 與 Kairos 0.3／0.4 可以任意混用 |
| Pulse 0.3.0 以上連 Kairos 0.2.0 | v2，不啟用 `durable`：連線期間照舊 at-most-once；斷線期間寫進緩衝的積壓保留到連上 Kairos 0.3.0 為止，日誌以 `INFO` 說明「伺服器不支援 durable…暫不補送」 |
| Pulse 0.5.0 連 Kairos 0.4（以前） | 手動憑證的主機與 0.4 相同。`pulse enroll` 需要 Kairos 0.5 的註冊端點：連不上 8444 時 `PLS-E2007`、該位址不是註冊端點時 `PLS-E2106`（結束代碼 5）；已註冊的主機照常連線，但沒有自動輪替（Kairos 不列出 `renew`）、過期續簽與除役通知 |
| Pulse 0.4（以前）連 Kairos 0.5.0 | 與 0.4 相同：Kairos 只在 hello 宣告 `renew`／`deregister` 時才在 `hello_ack` 列出並帶新欄位 |
| Pulse 0.8.0 連 Kairos 0.2.0～0.7.0 | v2＋`metrics`：19 個新的標準指標照送（舊版 Kairos 只檢查格式、原樣保存，升級後就能在趨勢中看到）；舊版 Kairos 不宣告 `processes`、`inventory`，Pulse 不送行程排行與資產 |
| Pulse 0.8.0 連 Kairos 0.8.0（`storage.backend = "jsonl"`） | 同上：JSONL 後端不宣告 `processes`、`inventory` |
| Pulse 0.2.0～0.7.0 連 Kairos 0.8.0 | 與以前相同（8 個指標）；Kairos 以磁碟總容量與可用空間推算使用率 |
| Pulse 0.9.0 連 Kairos 0.8.0（以前） | Pulse 宣告 feature `cluster`，舊版 Kairos 不列出：沒有節點名稱、不會收到排空（`drain`）。`server.fallback_addresses` 不依賴 Kairos 的版本，照常運作 |
| Pulse 0.8.0（以前）連 Kairos 0.9.0 | 與 0.8 相同：Kairos 只在 hello 宣告 `cluster` 時才帶 `server_node`、才送 `drain`；Kairos 關機時照舊直接關閉連線 |

更早的版本之間混用時可能出現下列問題：

| 組合 | 風險 |
|---|---|
| 舊版 Pulse（`44dcee5` 之前，即 `081f859` 及更早）連新版 Kairos | 舊版只把主機名稱中的控制字元替換成 `?`，`hostname` 含格式字元（Cf）時被以 `bad_request` 拒絕，**無法註冊**；舊版的讀取閒置固定為 `max(10 秒, 2 × min(heartbeat, report))`，心跳間隔較大時（例如 5 秒）小於「心跳間隔 + 伺服器最壞回應延遲 8 秒」，伺服器寫檔緩慢時**可能誤斷線** |
| 第一輪修正版 Pulse（`44dcee5`）連新版 Kairos | 新版 Kairos 另外禁止 U+034F、Hangul filler、變體選擇字元與私用區（見「協定概述」），`44dcee5` 不會替換這些字元；主機名稱含這些字元時**無法註冊** |
| 新版 Pulse 連舊版 Kairos（`572b955` 之前，`timeouts.write_secs` 沒有 6 秒上限） | Kairos 的 `timeouts.write_secs` 大於約 8 秒時，伺服器寫檔緩慢期間的回應延遲可能超過讀取閒置中的 10 秒餘裕，Pulse **誤判斷線**並重連 |

### 本版的行為改變（升級前請確認）

本版有數項會影響既有部署的行為改變（設定 `ca_cert` 時預設不再信任作業系統信任庫、預設心跳由 1 秒改為 15 秒使 Kairos 的離線判定由 5 秒變成 75 秒、啟動失敗的結束代碼依原因區分為 2／3／4 等），完整說明與升級步驟統一記錄在 monorepo 根目錄的 [CHANGELOG.md](../CHANGELOG.md)（發佈包中與本文件放在同一目錄）「破壞性變更與升級步驟」，這裡不再重複。

**部署順序**：協定 v2 起不必同時升級，任何順序都能以協定 v1 混用；仍建議先升級全部 Pulse，再升級 Kairos，以避開上表「舊版 Pulse 連新版 Kairos」的風險。新版 Pulse 送出前會替換所有新版 Kairos 禁止的字元，讀取閒置也只依心跳間隔計算，連舊版 Kairos（`timeouts.write_secs` 維持預設 5 秒時）不受影響。各版本組合見 CHANGELOG「版本相容矩陣」。

## 安裝（0.8.0 起：套件、一行安裝腳本、Windows MSI）

| 平台 | 方式 | 說明 |
|---|---|---|
| Linux x86_64／aarch64（Debian、Ubuntu、RHEL 系） | 一行安裝腳本 `pulse-install.sh` | 驗證 `SHA256SUMS` 的簽章與套件雜湊 → 安裝 deb／rpm → `pulse enroll --install-string` → 啟動服務（INSTALL.md 第 A.1 節） |
| 同上 | `apt install ./pulse-…-linux-<arch>.deb`／`dnf install ./….rpm` | 套件建立帳號 `pulse`、`/opt/pulse` 的目錄與 `agent.id`、啟用 `pulse.service`；之後 `sudo pulse enroll --install-string …`、`systemctl start pulse`（第 A.2 節） |
| Windows x86_64 | `msiexec /i pulse-…-windows-x86_64.msi` | 安裝到 `C:\Program Files\Pulse\` 並註冊服務 `Pulse`；之後 `pulse enroll --install-string …`（第 A.3 節）。**尚未在 Windows 實機驗證** |
| 其他、多實例、手動憑證 | tar.gz 發佈包 | 下方「從發佈包安裝」與 INSTALL.md 第 0～12 節 |

```bash
sudo PULSE_INSTALL_STRING='pulse-enroll:v1:…' sh pulse-install.sh   --base-url https://downloads.example.com/kairos-pulse/0.8.0 --allowed-signers /etc/kairos-pulse/allowed_signers
```

### Windows 服務（pulse service，0.8.0）

```powershell
pulse service install [--workdir <目錄>] [--start]   # 以系統管理員執行：註冊服務 Pulse（MSI 已註冊時不需要）
pulse service uninstall                              # 停止並刪除服務（不刪除任何檔案）
pulse service run [--workdir <目錄>]                 # 服務進入點，由服務控制管理員啟動
```

- 服務名稱 `Pulse`（`pulse status` 查詢的名稱）、自動啟動、LocalSystem。
- **失敗復原**：服務異常結束（含以非 0 結束代碼停止）時由服務控制管理員重新啟動——第 1 次失敗後等 10 秒、第 2 次 60 秒、第 3 次起每 300 秒，**沒有次數上限**（失敗次數超過動作數之後，服務控制管理員重複最後一個動作）；連續一天沒有失敗才重新從 10 秒算起。設定或憑證錯誤（結束代碼 3／4）因此會每 300 秒重試一次，直到修好為止（Linux 的 unit 對這些代碼不重新啟動，兩個平台不同）。服務每次啟動時比對完整的設定並校正回上述內容（由 MSI 註冊的服務、舊版註冊的服務、被修改過的設定都一樣；`sc.exe qfailure Pulse` 查看）。0.8.0～0.10.1 的文件寫「一天內最多 3 次」是錯的：那些版本設定的是三個「10 秒後重新啟動」，實際上每 10 秒重新啟動、沒有上限。「非 0 結束代碼也算失敗」這個旗標的變更依 Microsoft 的文件要到下次開機才生效，由 MSI 註冊的服務在第一次重開機前可能只對行程異常終止重新啟動（未實機驗證）。
- `service run` 先把目前目錄切換到 `--workdir`（預設為 `pulse.exe` 所在目錄），**不會落在 `C:\Windows\System32`**：`pulse.toml`、`agent.id`、`identity\`、`buffer\` 與日誌 `logs\pulse-YYYY-MM-DD.log` 都在這個目錄下；本機狀態檔為 `%ProgramData%\Pulse\status.json`。
- 服務的「停止」與「關機」要求與 Ctrl+C 相同：送出 TLS close_notify 後結束；結束代碼以服務特定代碼回報（`sc.exe query Pulse` 的 `SERVICE_EXIT_CODE`，1／3／4 與 Linux 相同）。
- 錯誤代碼：`PLS-E8105` 服務無法安裝、移除或啟動（多半是沒有系統管理員權限）；`PLS-E8106` `service run` 不是由服務控制管理員啟動，或工作目錄不存在。
- 只在 Linux 上以 `x86_64-pc-windows-gnu` 交叉編譯與 clippy 檢查，**未在 Windows 實機執行**；Windows 事件記錄尚未實作。

## 從發佈包安裝

地端正式部署也可以使用預編譯的發佈包 `pulse-<版本>-<commit>-linux-<arch>.tar.gz`（x86_64 或 aarch64、musl 靜態連結，不依賴系統函式庫），不需要安裝 Rust。發佈包內含執行檔、`pulse.example.toml`（內建預設值的參考，與 repo 的 `config.toml` 相同）、`pulse.service`（系統層級 systemd unit 範本）、`INSTALL.md`、本文件、`PROTOCOL.md`、`CHANGELOG.md`（變更紀錄與升級步驟）、`THIRD-PARTY-LICENSES`（第三方授權聲明）與 SBOM（CycloneDX）。

完整步驟見 [`packaging/INSTALL.md`](packaging/INSTALL.md)（發佈包根目錄的 `INSTALL.md`），概要如下。所有步驟請在**同一個 root shell**（`sudo -i`）中依序執行，不能只在每個指令前加 `sudo`：`/opt/pulse` 的權限是 750，一般帳號無法 `cd` 進去，而 `agent.id`、私鑰與 CSR 都寫到目前目錄，`AGENT_ID` 變數也要在步驟間延續。

1. 以 `ssh-keygen -Y verify` 驗證 `SHA256SUMS.sig`（發佈公鑰 `allowed_signers` 須從另一個管道取得）、`sha256sum -c --ignore-missing SHA256SUMS` 驗證雜湊後解壓（`tar xzf … --strip-components=1`），`./pulse --version` 確認版本與 commit。
2. 建立系統使用者 `pulse` 與 `/opt/pulse`、`/opt/pulse/logs`，安裝執行檔。
3. `cd /opt/pulse` 後執行 `./pulse --print-agent-id` 產生 `agent.id`；在同一目錄產生私鑰與 CSR，把 CSR（root 600，需以 root 複製出來）交給 Kairos 管理者以 `openssl ca` 簽發（見「啟用 mTLS」）。
4. 建立只含覆蓋欄位的 `pulse.toml`，放好 `ca.pem`、`client.pem`、`client.key` 並設定權限。
5. 安裝 `pulse.service`（`WorkingDirectory=/opt/pulse`、`Restart=always`、`KillSignal=SIGINT`、`StandardOutput=null`：日誌只保留 `logs/` 一份，stderr 仍進 journal），`systemctl enable --now pulse`。
6. 確認日誌出現「已向伺服器註冊」，以及 Kairos 的 `agents.json` 中此 agent 為 `online`。

- **監控**：憑證被撤銷、`auth_failed`、伺服器憑證不受信任或名稱不符等需要人工處理的錯誤，Pulse 會持續重連而不結束，服務仍為 `active (running)`。請看 `systemctl status pulse` 的 `Status:` 行（例如「已註冊：127.0.0.1:18443」「無法註冊：certificate_rejected（…）」「無法連線：伺服器憑證名稱不符（…）」「重新連線中」；需要 unit 的 `NotifyAccess=main`，舊版 unit 沒有這行時不會顯示）、`journalctl -u pulse -p err`、`logs/` 或 Kairos 的 `agents.json`，見 INSTALL.md 第 8.1 節與下方「日誌」。
- **多實例**：同一台主機跑多個 Pulse 時，每個實例使用自己的 `/opt/pulse-N`（`agent.id`、憑證、`pulse.toml`、`logs/` 各自一份），執行檔共用 `/opt/pulse/pulse`；unit 只改 `WorkingDirectory` 與 `ReadWritePaths` 兩行。完整指令見 INSTALL.md 第 9 節。
- **移除**：有多實例時先停用並移除所有 `pulse-N.service`，`daemon-reload`、`reset-failed` 後，最後才刪除 `/opt/pulse` 與使用者 `pulse`（見 INSTALL.md 第 12 節）。

從 0.4 升級到 0.5 時**先升級 Kairos**（建立簽發 CA、開放 8444），再升級 Pulse 並建立 `identity/` 目錄（CHANGELOG [0.5.0]「升級步驟」、INSTALL 第 10 節）；更早的版本見上一節。發佈包由 [`packaging/build-release.sh`](packaging/build-release.sh) 產生：以 docker 的 `rust:1.99-alpine`（以 `@sha256:` digest 固定）建置 Linux x86_64／aarch64 的 musl 靜態執行檔與 Windows x86_64 執行檔，輸出到 monorepo 根目錄的 `dist/`（tar.gz、deb、rpm、zip、msi、SBOM、`pulse-install.sh`，並更新與 Kairos 共用的 `SHA256SUMS`）；需要 Docker，Windows 請在 Git Bash 執行 `sh packaging/build-release.sh`。正式發佈的流程見 monorepo 根目錄的 `RELEASING.md`。

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
[INFO] 已向伺服器註冊（agent_id=…，伺服器離線判定 75000 ms；TLS 加密連線 127.0.0.1:8080，domain=localhost，TLSv1_3，已驗證伺服器憑證）
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
- 設定了 `ca_cert` 時，Pulse 預設**只信任 `ca_cert`**，不信任作業系統信任庫（`server.use_system_roots` 未設定時的預設），避免任何公開 CA 簽發的同名憑證也被接受；不需要另外設定 `use_system_roots = false`。
- 伺服器憑證到期時只要用 CA 重簽，Pulse 端不必改設定。
- `ca.key` 是整個信任鏈的根，務必離線保存。
- 由公開 CA 簽發的伺服器憑證不需要設定 `ca_cert`：沒有 `ca_cert` 時 Pulse 預設信任作業系統的信任庫。同時設定了 `ca_cert` 又需要信任公開 CA 時，明確設定 `server.use_system_roots = true`。

## 自動註冊（pulse enroll，0.5.0）

需要 Kairos 0.5 以上、註冊端點（預設 8444）為「開放」。規格：`crates/kairos-proto/ENROLL.md`；安裝步驟：INSTALL 第 5.0 節。

```bash
sudo pulse enroll --install-string-file - < install-string.txt   # 建議：安裝字串不出現在命令列（ps）
sudo pulse enroll --install-string 'pulse-enroll:v1:eyJzIjoia2Fpcm9z…'   # 相容，但不建議
sudo pulse enroll --server kairos.example.gov.tw --token KRT1-7G4Q-M2XD-9PBA-H6WN --ca-pin sha256:3f9a…
pulse enroll --status          # 目前的註冊狀態
sudo pulse enroll --cancel     # 撤回等待中的申請、刪除本機未使用的私鑰
```

流程：連線到註冊端點（只信任 CA 指紋、`--ca-file` 或人工確認的 CA，**不使用作業系統信任庫**；根 CA 可以是 P-256、P-384 或 RSA）→ `server-info`、`token-check`（token 有問題時不產生任何檔案）→ 在本機產生 ECDSA P-256 私鑰（`identity/request.key`，**不會離開主機**）、CSR 與 32 bytes 的輪詢憑據，先寫入 `identity/enrollment.json` 再送出 → 自動核准時當場取得憑證；需核准時顯示**申請編號**並輪詢（Kairos 支援時長輪詢，否則依 `poll_after_ms` 每次 ×1.5、上限 5 分鐘、±20%）→ 確認 SAN 為 `DNS:<agent_id>`、與私鑰成對、`ca_pem` 與指紋相同後原子寫入 `identity/client.pem`、`client.key`、`ca.pem` → 更新 `pulse.toml`（保留原有內容與註解，備份 `pulse.toml.bak-<時間>`）→ 以新憑證送一次 `hello` 確認，服務在執行時重新啟動它。

| 檔案 | 權限 | 內容 |
|---|---|---|
| `identity/client.key`、`request.key`、`grace.key`、`client.next.key` | 0600（Windows 只有 SYSTEM 與 Administrators） | 私鑰（PKCS#8） |
| `identity/enrollment.json` | 0600 | 申請的狀態、`request_id`、輪詢憑據、下次輪詢／重送時間、過期續簽的進度；申請**建立前**暫存 token（排隊或時鐘偏差時由服務重送），Kairos 受理後立即刪除 |
| `identity/client.pem`、`ca.pem` | 0640 | 用戶端憑證（含中繼）、Kairos CA |

寫入一律「暫存檔 → fsync → rename → fsync 目錄」；以 root 執行時新檔案改為 `identity/` 目錄的擁有者（服務帳號）。`pulse enroll` 與服務以 `identity/.lock` 互斥。

**可靠性**：中斷（Ctrl+C、斷電）後重新執行或服務啟動時從 `enrollment.json` 接續，不重送新申請、不產生第二組私鑰；回應遺失後的重送沿用同一個 CSR 與輪詢憑據，Kairos 回傳同一筆申請。429／503 依 `Retry-After × U(1.0, 1.2) + U(0, 2 秒)` 排隊（絕不早於伺服器指定的時間，`PLS-E2202`／`PLS-E3309`），網路錯誤 2 秒起倍增、上限 5 分鐘。送出申請之前（`server-info`、`token-check`）收到 429／503 時（0.9 起）也依同一個公式等候後重試：合計不超過 `--wait`（沒有指定時 600 秒），互動時在 stderr 顯示倒數，Ctrl+C 取消（`PLS-E2202`，尚未產生任何檔案）；`--no-wait` 不等候，來源被封鎖（`source_blocked`，`PLS-E3005`）不等候。Kairos 因時鐘偏差暫不受理（`clock_skew`，`PLS-E8302`）時保留私鑰與 CSR，每 5 分鐘（±20%）以新的 `sent_at` 重送，偵測到本機時間被調整（牆鐘與單調時鐘的差距變動超過 1 秒）時立即重送。

**服務**：沒有可用的身分而 `enrollment.json` 有進行中的申請時，服務先完成申請（依 `next_poll_at` 恢復，不立即輪詢）再連線；申請已結束（被拒、逾期、token 無效…）時不連線，`pulse status` 為嚴重，等待新的 `pulse enroll`。沒有 `enrollment.json` 的既有部署（手動憑證、未啟用 mTLS）行為不變。

**憑證更新與過期**（身分由 `pulse enroll` 管理時）：

- 正常輪替（8443 feature `renew`）：已使用壽命 2/3 或剩餘 `identity.renew_before_days`（預設 30）天時，在既有連線送 `renew_request`（新私鑰在 `client.next.key`，同一個 CSR 重送時沿用）；取得 `renew_result` 後以新憑證另建連線 `hello` 成功才取代 `client.pem`／`client.key`。需人工核准等失敗時 `PLS-W1111`、1 → 12 小時退避，繼續使用舊憑證。Kairos 不支援時不輪替（0.4 的行為）。
- 過期寬限期續簽：Kairos 以 TLS alert `CertificateExpired` 確認過期後（不單憑本機時鐘），經 8444 `POST /v1/enroll/renewals` 以**舊私鑰**對含 `sent_at` 的簽章內容簽章（`kairos-proto` 的 `grace_renew_signing_input`），`sent_at` 不重複使用；`replayed_request` 立即重新簽章重送一次，仍被拒時 `PLS-W2107`。受阻時依 2 → 60 分鐘退避並對 8443 出示過期憑證報到；`grace_exceeded` 每 6 小時、`grace_clock_rejected` 每 5 分鐘、`grace_rejected` 停止（服務重新啟動時再試）。
- 除役：安裝程式完全移除時執行 `pulse decommission --initiated-by <msi|install_script|package_manager>`：已註冊時經 8443（`hello` 帶 `intent: "deregister"`）送 `deregister`；等待核准中時經 8444 撤回申請（以申請時的私鑰對一次性 nonce 簽章）。最多 15 秒、重試 1 次，失敗時 `PLS-W3107`／`PLS-W3310`（結束代碼 8）。

`pulse status` 的「連線」「身分」列與 `--json` 的 `data.identity.state`、`identity.enrollment`、`identity.grace` 呈現上述狀態（設計包 `enrollment-ux.md` §4.2），`--brief` 例如 `pending   plc-hmi-07 pending_approval PLS-I3307`、`critical  plc-hmi-07 clock_rejected offset=-7m42s PLS-E8302`。

### 實作現況（0.5.0 整合）

- 8443 的訊息（`renew_request`／`renew_result`／`deregister`／`deregister_ack`、`hello.intent`、`hello_ack.expired_renew_grace_days`／`renew_now`、錯誤碼 `renew_requires_approval`／`invalid_csr`／`rate_limited`）使用 `kairos-proto` 的共用型別（PROTOCOL.md「憑證輪替與除役」）；合併前暫用的 `pulse/src/lifecycle.rs` 已刪除。
- hello 只在身分由 `pulse enroll` 管理時宣告 `renew`、只在 `pulse decommission` 的一次性連線宣告 `deregister`；手動憑證的主機 hello 與 0.4 相同。
- 端對端測試：`tests/e2e/tests/enroll_pulse.rs` 以實際的 `kairos` 與 `pulse` 執行檔跑 ENROLL.md 第 19.3 節的情境（第 19.5 節列出對應）；測試用的假 8444 伺服器（`pulse/src/enroll/fake.rs`）保留作單元測試。

## 啟用 mTLS（客戶端憑證）

以 `pulse enroll` 註冊（上一節）時不需要這一節的手動步驟。

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
| TLS 握手完成前連線就被關閉（例如超過 Kairos 的每 IP 連線上限 `server.max_connections_per_ip`：舊版 Kairos 或未啟用 mTLS 時多台代理經同一個 NAT 連入常見，新版 Kairos 啟用 mTLS 時只計算握手前與握手中的連線，大量代理同時重連時仍可能發生；或超過新連線速率、伺服器握手中的連線數上限） | 伺服器在 TLS 握手完成前就關閉了連線：可能超過伺服器的每 IP 連線上限（同一 IP 位址的連線過多）、新連線速率過快（同一 IP，IPv6 為同一 /64）或伺服器握手中的連線過多，或伺服器正在重新啟動、連到的連接埠不是 TLS 服務 |

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

測試都寫在各模組內（`#[cfg(test)]`），包含以 `testdata/` 的測試憑證進行的實際 TLS 握手測試。協定訊息的序列化、解析與欄位驗證（含直接從 `crates/kairos-proto/PROTOCOL.md`「範例」節解析的 v1、v2 測試向量與版本協商）在與 Kairos 共用的 `crates/kairos-proto` 中測試；本產品另測試 hello 的版本範圍與主機資訊、依 `hello_ack` 選擇 report 格式（舊版 Kairos → 三欄位、新版 → 通用指標、版本不符 → 斷線）、指標的篩選與上限、超過單行上限時的捨去、主機指紋與規格的計算範例一致、時鐘偏差警告。日期保護的核心規則在 `crates/date-guard`；在 monorepo 根目錄執行 `cargo test -p kairos-proto -p date-guard`，或以 `cargo test --workspace` 一併執行；`cargo test`（在 `pulse/` 目錄）只執行本產品的測試。Unix 專屬的部分（檔案權限、sd_notify 等）以 `#[cfg(unix)]` 標示，只在 Linux／macOS 上執行，Windows 專屬的 ACL 檢查同理。實際數量以 `cargo test` 的輸出為準（各平台不同），文件不列出數字。

> **`testdata/` 中的私鑰（`*.key`）只供測試使用，已公開在 repo 中，絕對不可用於任何部署環境。** 正式環境的憑證與私鑰請依「私有 CA」與「啟用 mTLS」重新產生。

建議在 CI 加上 `cargo audit`（需先 `cargo install cargo-audit`），檢查依賴套件是否有已公告的安全漏洞。

命令列參數：

| 參數 | 說明 |
|---|---|
| （無）、`run` | 啟動監控代理 |
| `status [選項]` | 顯示本機狀態（見「本機狀態（pulse status）」；`pulse status --help`） |
| `explain <代碼>` | 印出錯誤代碼的標題（例如 `pulse explain PLS-W4202`），完整對照見 `ERROR-CODES.md` |
| `--print-agent-id` | 印出本機 agent_id 後結束（見「啟用 mTLS」） |
| `-V`, `--version` | 印出 `pulse <版本> (<commit>)` 後結束（不讀取設定，設定有誤時也能確認版本） |
| `-h`, `--help` | 顯示使用說明 |

未知參數會印出錯誤（`PLS-E0001`）與用法，並以 exit code 2 結束。

`--version` 的 commit 在建置時決定：環境變數 `BUILD_COMMIT`（`packaging/build-release.sh` 會傳入）→ `git rev-parse --short HEAD` → 都取不到時為 `unknown`。啟動日誌的第一行也會印出同樣的版本字串；送給伺服器的 hello 中的 `agent_version` 仍只有版本號（例如 `0.1.0`）。

程式啟動後會：

1. 載入並驗證設定；設定值超出範圍（含 `server.port` 為 0、`server.address` 或 `server.domain` 空白）、`log.level` 無效、明確設定 `use_system_roots = false` 卻未設定 `ca_cert`（`accept_invalid_certs = true` 時例外）、`ca_cert` 讀不到、客戶端憑證設定不完整或無法載入（含私鑰加密）、`agent.id` 內容不合法（含超過 1024 bytes、非 UTF-8）或無法建立、客戶端憑證 SAN 與 agent_id 不符時，**立即**印出錯誤並以非 0 結束（客戶端憑證已過期**不會**阻止啟動，見「啟用 mTLS」）。結束代碼依原因區分（依設計包 `cli-spec.md` §1.3）：設定本身的錯誤（`pulse.toml`／環境變數的語法、型別、範圍錯誤，`log.level` 無效，`server.domain` 不是有效的 DNS 名稱或 IP 位址，`use_system_roots = false` 卻未設定 `ca_cert`）為 **3**；憑證錯誤（`ca_cert`、客戶端憑證或私鑰讀不到、格式錯誤、不成對，SAN 與 agent_id 不符，`agent.id` 內容不合法或無法讀取、建立）為 **4**；`pulse.service` 以 `RestartPreventExitStatus=2 3 4` 不再重試。日誌目錄無法建立等其餘啟動失敗為 1，仍由 systemd 重新啟動。日誌初始化之後才發生的啟動失敗（例如憑證錯誤），除了寫日誌，也另寫一行 `[ERROR] …` 到 stderr，`pulse.service` 的 journal 看得到原因
2. 清除過期日誌檔（清除前檢查系統日期是否合理，見「日誌」；之後每小時一次），並以 `INFO` 記錄工作目錄、`pulse.toml` 的完整路徑與是否讀到（工作目錄是 `System32`／`SysWOW64` 時另記 `WARN`，見「設定」）
3. 讀取（或第一次產生）`agent_id`
4. 印出一次系統資訊（在背景執行緒進行；收到 Ctrl+C 時不會等它完成）。`info` 等級下 CPU 只有一行摘要（例如 `CPU: 24 個核心，平均使用率 37.38%（各核心使用率見 debug 等級）`），磁碟略過虛擬檔案系統與容量為 0 的掛載點，以及同一磁碟的重複掛載點（「磁碟名稱＋檔案系統＋總容量」相同者只列一次，保留路徑最短的掛載點，例如 systemd 沙箱的 `ReadWritePaths=`、`PrivateTmp=` bind mount 或 btrfs subvolume；磁碟名稱為空時不合併），網路略過 `lo` 與虛擬介面（`veth*`、`docker*`、`br-*` 等），略過的數量各以一行統計；完整清單需 `log.level = "debug"`。磁碟容量以十進位單位顯示：1 GB 以上為 GB（小數 1 位，例如 `39.4 GB`），未滿 1 GB 為整數 MB（例如 `535 MB`）。取不到的系統名稱、核心版本、主機名稱等顯示「未知」；沒有溫度感測器（例如 VM）時印「未偵測到溫度感測器」
5. 啟動系統資訊收集背景任務（先建立 CPU 使用率的基準，第一次取樣在一個 `report.interval` 之後，之後每 `report.interval` 秒一次）
6. 連線到伺服器、送出 hello、收到 hello_ack 後開始送 heartbeat 與 report

按 Ctrl+C 結束（Windows 上 Ctrl+Break、關閉主控台視窗、系統關機也一樣；其他平台為 SIGINT、SIGTERM）：

- 已連線時，會**盡力**先對伺服器送出 TLS close_notify（最多等 1 秒），讓 Kairos 看到正常關閉，而不是連線被重設。Windows 關閉主控台、登出或關機時只給行程有限的時間（一般互動式程式也不一定收得到關機事件），Unix 上的 SIGKILL 也無法攔截，這些情況下 close_notify 不一定送得出去。
- 若收到結束要求時有一則訊息只送出一部分，Pulse **不送** close_notify、直接關閉連線，讓伺服器把它當成未正常關閉的連線並丟棄不完整的行，而不是把半行當成最後一則訊息處理。
- 啟動階段（例如系統資訊收集因 WMI 很慢而卡住、建立 TLS 設定時讀檔很慢）也能立即中斷：Pulse 不會等待仍在背景執行緒上的系統呼叫，收到訊號後約 1 秒內結束。
- 正常結束時 exit code 為 0；收尾期間再送一次訊號會強制結束，exit code 為 130。

結束代碼總覽：0 正常結束；1 內部錯誤或未分類的啟動失敗；2 用法錯誤（未知參數）；3 設定錯誤；4 憑證錯誤（含 `agent.id`）；130 強制結束（與 Kairos 共用的對照表見 monorepo 根目錄 README「結束代碼」）。

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

> **以 Windows 服務執行時**（0.8.0 起以 `pulse service run` 或 MSI 註冊），服務先把工作目錄切換到 `pulse.exe` 所在目錄（或 `--workdir`），`pulse.toml`、`agent.id` 與 `logs/` 都在那裡。以其他服務包裝工具執行舊的命令列時，預設工作目錄仍是 `C:\Windows\System32`：Pulse 偵測到工作目錄是 `System32` 或 `SysWOW64` 時會記 `WARN`，請改用 `pulse service install`（見「Windows 服務」）。

環境變數的值一律以字串讀入：數值與布林欄位在載入時才轉成對應型別，路徑、`domain`、`address` 等字串欄位保持原樣（例如 `PULSE__AGENT__ID_FILE=0001` 就是 `0001`，不會被改寫成 `1`）。

```toml
[server]
address = "127.0.0.1"          # 伺服器位址（第一優先）
port = 8080                    # 伺服器連接埠
fallback_addresses = []        # 主位址連不上時依序改試的其他 Kairos 節點（"主機" 或 "主機:埠"，最多 16 個；見「多個伺服器位址與排空」）
domain = "localhost"           # TLS SNI / 憑證名稱驗證用的名稱（DNS 名稱或 IP；須與伺服器憑證 SAN 相符）
accept_invalid_certs = false   # 是否略過伺服器憑證驗證（預設 false）
ca_cert = ""                   # 信任的 CA / 自簽憑證 PEM（可含多張）；空字串 = 只用作業系統信任庫
# use_system_roots = true      # 預設不寫：有 ca_cert 時只信任 ca_cert，沒有時信任作業系統信任庫；明確設定時以設定為準
client_cert = ""               # mTLS 客戶端憑證 PEM（可接中繼 CA）；空字串 = 不出示客戶端憑證
client_key = ""                # 對應私鑰 PEM（PKCS#8 / PKCS#1 / SEC1，不可加密）；須與 client_cert 同時設定

[retry]
max_retries = 5                # 連續失敗達此次數後，重連日誌由 warn 升級為 error（重試不設上限）
initial_delay = 2              # 第一次重連延遲（秒），之後每次加倍
max_delay = 60                 # 重連延遲上限（秒）

[heartbeat]
interval = 15                  # heartbeat 間隔（秒）；伺服器的離線判定為 max(5 × interval, 5 秒)

[report]
interval = 5                   # 系統資訊回報間隔（秒）

[agent]
id_file = "agent.id"           # 存放 agent_id 的檔案
display_name = ""              # 主控台顯示名稱（協定 v2 的 hello；空字串 = 不送）
tags = []                      # 標籤，例如 ["env:prod", "team:web"]（協定 v2 的 hello；只能在 pulse.toml 設定）

[buffer]
enabled = true                 # 本機緩衝（at-least-once）；false = 與 0.2.0 相同，不寫磁碟、不補送
dir = "buffer"                 # 緩衝目錄（相對於工作目錄）
max_mb = 128                   # 待送資料的總大小上限（MiB；0.8.0 起預設由 64 改為 128）
max_records = 50000            # 待送資料的筆數上限（與 max_mb 先到者為準，滿時丟掉最舊的）
max_age_hours = 168            # 取樣超過這麼久（小時）的資料不再送出
replay_interval_ms = 1200      # 兩筆 report 之間的最短間隔（毫秒；補送積壓的速率，不超過 report.interval 的 60%）

[collect]                      # 標準指標的採集範圍（0.8.0 起；見「回報資料格式」）
filesystems = true             # system.filesystem.*（每個掛載點：total、available、used、usage）
disk_io = true                 # system.disk.*（累計讀寫量；Linux 另有讀寫次數與忙碌時間）
network = true                 # system.network.*（累計收發位元組、封包、錯誤）
temperature = true             # system.temperature
max_filesystems = 16           # 掛載點上限（0～16；0 = 不送）
max_disks = 8                  # 磁碟 I/O 裝置上限（0～8）
max_interfaces = 16            # 網路介面上限（0～16）
interface_exclude = []         # 額外排除的介面（glob，* 與 ?，例如 ["tap*", "wg?"]；只能在 pulse.toml 設定）

[processes]                    # 行程數與行程排行（0.8.0 起）
enabled = true                 # false = 不宣告 processes、不送 system.process.count
interval = 60                  # 行程排行的間隔（秒，1～3600；實際不小於 report.interval）
top = 5                        # 依 CPU、依記憶體各取幾個（1～10）
command = "none"               # 命令列：none（不送）／executable（只送執行檔路徑）／full（含參數，截斷到 1024 bytes）
include_user = false           # 送使用者名稱：本版尚未實作，設為 true 只記一則 WARN

[inventory]                    # 主機資產（0.8.0 起）
enabled = true                 # false = 不宣告 inventory、不收集
interval = 3600                # 重新收集的間隔（秒，300～86400）；內容改變才送，滿 24 小時也送一次
hardware_serials = true        # 送系統序號（Linux 需要 pulse.service 的 ExecStartPre=，見 INSTALL.md）
network_addresses = true       # 送網卡的 MAC 與 IP 位址

[status]
file = ""                      # 本機狀態檔；空字串 = /var/lib/pulse/status.json（Windows %ProgramData%\Pulse\status.json）

[log]
level = "info"                 # 日誌級別：trace / debug / info / warn / error（不分大小寫）
retention_days = 14            # 日誌檔保留天數（今天與前 N 天）；0 = 永久保留
format = "text"                # text（RFC 3339 帶時區）／json（logs/pulse-YYYY-MM-DD.jsonl）／legacy（0.5 以前的格式）
timezone = "local"             # text 的時間：local 或 utc（json 一律 UTC）
stdout = true                  # 同時寫到 stdout

[log.syslog]
target = ""                    # udp://主機:埠、tcp://主機:埠、unix:///dev/log；空字串＝不外送
facility = "daemon"
app_name = "pulse"
payload = "text"               # text 或 json

[metrics]                      # 本機的 Prometheus 指標（見「本機指標」），預設關閉
listen = ""                    # 例如 "127.0.0.1:9465"（只接受 loopback）
textfile = ""                  # 例如 "/var/lib/node_exporter/textfile_collector/pulse.prom"
textfile_interval = 15

[remote_config]                # 集中設定（0.9.0；見「集中設定與遠端升級」）
enabled = true                 # false = 不接受 Kairos 下發的設定（只能在本機設定）

[upgrade]                      # 遠端升級（0.9.0，Linux；只能寫在 pulse.toml，環境變數只能關閉）
enabled = true                 # false = 不接受 Kairos 推送的升級
allowed_signers = ""           # 信任錨；空字串 = /etc/kairos-pulse/allowed_signers（root 擁有、其他人不可寫）
extra_principals = []          # 除 release@kairos-pulse 外另外接受的 principal（以自己的金鑰重新簽署時）
allow_downgrade = false        # 接受遠端降版（推送也必須允許）；自動回滾不受影響
url_allowlist = []             # 允許從外部 URL 下載發佈檔的前綴（0.10.0；空 = 只從 Kairos 下載）
external_ca = ""               # 外部 URL 的 CA（PEM；設定時只信任它，空 = 作業系統信任庫）
health_timeout_secs = 0        # 替換後的健康確認期限（0 = 依 Kairos；非 0 時取兩者較大，120～3600）
```

`server.use_system_roots` 也可用環境變數設定（例如 `PULSE__SERVER__USE_SYSTEM_ROOTS=true`）。未設定時的預設值依 `ca_cert` 決定：

| `ca_cert` | `use_system_roots` | 信任的伺服器憑證 |
|---|---|---|
| 未設定 | 未設定 | 作業系統信任庫 |
| 已設定 | 未設定 | 只有 `ca_cert`（不信任作業系統信任庫） |
| 已設定 | `true` | 作業系統信任庫 + `ca_cert` |
| 任意 | `false` | 只有 `ca_cert`（`ca_cert` 必填） |

啟動時以 `INFO` 記錄實際採用的信任來源，例如「伺服器憑證信任來源：只信任 server.ca_cert（…）」。

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
| `log.format` | `text` / `json` / `legacy` | 不分大小寫；見「日誌」 |
| `log.timezone` | `local` / `utc` | 只影響 `text` |
| `log.syslog.target` | 空字串、`udp://主機[:埠]`、`tcp://主機[:埠]`、`unix:///絕對路徑` | IPv6 位址以 `[ ]` 包住；`unix://` 只在 Unix 平台 |
| `log.syslog.facility` | `daemon`、`user`、`auth`、`authpriv`、`local0`～`local7` | |
| `log.syslog.payload` | `text` / `json` | |
| `metrics.listen` | 空字串或 loopback 的 `IP:埠` | 不接受 `0.0.0.0` 等對外位址（本機指標不需要認證） |
| `metrics.textfile` | 空字串或副檔名 `.prom` 的路徑 | node_exporter 的 textfile collector 只讀 `*.prom` |
| `metrics.textfile_interval` | 5～3600 秒 | |
| `agent.id_file` | 非空字串 | |
| `agent.display_name` | 空字串，或 1～255 bytes | 不得含控制字元、格式字元等不可見字元（規則同 Kairos 對 `hostname` 的限制）；只在 Kairos 支援協定 v2 時送出 |
| `agent.tags` | 最多 32 個，每個 1～64 bytes，不得重複 | 字元限制同上；只能在 `pulse.toml` 設定（環境變數不支援陣列） |
| `server.domain` | 有效的 DNS 名稱或 IP 位址，不可空白 | |
| `server.use_system_roots` | `true` / `false` / 不設定 | 不設定時依 `ca_cert` 決定（見上表）；明確設為 `false` 時 `server.ca_cert` 必填，否則啟動即報錯（`accept_invalid_certs = true` 時不使用信任庫，不檢查此項） |
| `server.client_cert` / `server.client_key` | 同時設定或同時留空 | 只設定一個時啟動即報錯 |
| `buffer.dir` | 非空白字串（`buffer.enabled = true` 時） | |
| `buffer.max_mb` | 1～102400 MiB | |
| `buffer.max_records` | 100～10000000 | |
| `buffer.max_age_hours` | 1～8760 小時 | |
| `buffer.replay_interval_ms` | 500～60000 毫秒 | 下限 500 毫秒對應 Kairos 的 report 速率限制（每秒 2 則）。實際使用的值不超過 `report.interval` 的 60%（否則積壓補不完；被調整時啟動記 `INFO`） |
| `status.file` | 空字串或路徑 | 空字串為預設位置（見「本機狀態（pulse status）」） |
| `collect.max_filesystems` | 0～16 | 0 = 不送 `system.filesystem.*`。上限不可再提高：每筆 report 最多 256 個指標，Kairos 0.7.0 以前超過就整筆拒絕 |
| `collect.max_disks` | 0～8 | 同上，`system.disk.*` |
| `collect.max_interfaces` | 0～16 | 同上，`system.network.*` |
| `collect.interface_exclude` | 最多 32 個，每個 1～256 bytes 的 glob | 只能在 `pulse.toml` 設定 |
| `processes.interval` | 1～3600 秒 | 小於 `report.interval` 時以 `report.interval` 為準（不報錯） |
| `processes.top` | 1～10 | |
| `processes.command` | `none` / `executable` / `full` | 見「回報資料格式」的隱私說明 |
| `inventory.interval` | 300～86400 秒 | |
| `upgrade.health_timeout_secs` | 0，或 120～3600 秒 | 0 = 依 Kairos 推送的值（預設 600） |
| `upgrade.extra_principals` | 每項 1～256 bytes，不含空白與逗號 | |
| `upgrade.url_allowlist` | 每項以 `https://` 開頭 | 0.10.0：外部 URL 的版本庫。以目錄比對；不合法的項目（查詢字串、帳號密碼…）不比對任何 URL，啟動時記 `WARN` |

### ⚠️ 安全性提醒

- `accept_invalid_certs` **預設為 `false`**，Pulse 會驗證伺服器憑證是否受信任、`server.domain` 是否與憑證 SAN 相符。
- 使用自簽憑證或私有 CA 時，請用 `ca_cert` 指定要信任的憑證，**不要**改成 `accept_invalid_certs = true`；設定了 `ca_cert` 時預設只信任它，不需要另外設定 `use_system_roots`。
- `accept_invalid_certs = true` 會略過伺服器憑證的信任鏈與名稱驗證，任何人都能冒充伺服器（中間人攻擊），僅限臨時除錯使用。啟用時啟動階段會以 `ERROR` 記錄警告（`log.level` 最高只能設為 `error`，因此一定看得到），每次連線時，連線當下（「已建立 TLS 加密連線」）與註冊成功時（「已向伺服器註冊」）各記一行 `WARN`，並標註「⚠️ 未驗證伺服器憑證」。握手簽章仍會正確驗證（確保對方持有該憑證的私鑰），但這無法防止中間人使用自己的憑證。若同時設定了 `ca_cert`，也會以 `ERROR` 警告 `ca_cert` 此時形同無效。
- 未啟用 mTLS 時，`agent.id` 的內容就是本機在伺服器上的身分憑據，請妥善保護（見「agent.id」）。

## 協定概述

完整規格見 [PROTOCOL.md](PROTOCOL.md)，以下為摘要：

- 一條長 TLS 連線，**NDJSON**（UTF-8，一行一則 JSON，以 `\n` 結尾），單行上限 65536 bytes
- 每則訊息有 `type` 與 `seq`；`seq` 每條連線從 1 開始遞增，重連後重新計算
- 流程：Pulse 送 `hello` → Kairos 回 `hello_ack` → 之後 Pulse 每 `heartbeat.interval` 秒送 `heartbeat`、每 `report.interval` 秒送 `report`，Kairos 對每則訊息回 `ack`
- **版本協商**：hello 送 `"proto":1,"proto_min":1,"proto_max":2,"features":["metrics","durable","inventory","processes"]`（舊版 Kairos 只看 `proto`；`buffer.enabled = false` 時不宣告 `durable`，`inventory.enabled`／`processes.enabled = false` 時不宣告對應的 feature）。本機狀態檔（`status.json`、`pulse status --json`）的 `features` 列出雙方都宣告的部分。`hello_ack` 的 `proto` 是協商結果：1 → report 為三個欄位；2 且 Kairos 也宣告 `metrics` → report 為通用指標。`proto` 不在 1～2 時斷線並記 `ERROR`（`proto_mismatch`），Kairos 回 `unsupported_version` 時同樣以 `ERROR` 記錄（`message` 含 Kairos 支援的範圍）
- **hello 的主機資訊**（v2，舊版 Kairos 忽略）：`host_fingerprint`（機器 ID 的 SHA-256，不送原始值，計算方式見 PROTOCOL.md「主機指紋」；Linux 讀 `/etc/machine-id`、Windows 讀 `MachineGuid`，讀不到時不送）、`display_name` 與 `tags`（設定 `agent.display_name`、`agent.tags`，未設定時不送）、`report_interval_ms`（`report.interval`）、`os`（sysinfo 取得的作業系統名稱、版本、核心版本與 CPU 架構）
- **時鐘檢查**：v2 的 `hello_ack` 帶 Kairos 的 `server_time`。與本機時鐘相差 30 秒～5 分鐘記 `WARN`（`clock.skew PLS-W8301`），超過 5 分鐘記 `ERROR`（`clock.skew PLS-E8302`，同時寫 stderr），請檢查 NTP。只在等級改變或偏差再變化超過 1 分鐘時記錄（重連時不重複），回到 30 秒以內記一則 `INFO`（`clock.skew -`）
- `report` 的 `ack` 代表該筆資料已寫入伺服器的資料庫或資料檔；伺服器確定沒有寫入時改回非 fatal 的 `storage_unavailable`（message 為 `report could not be stored`：寫入失敗或達到伺服器的每日檔案大小上限；或 `report rate limit exceeded`：同一來源送得太快）。Pulse 收到時記 `WARN`（節流，見「日誌」），連線保持。伺服器無法確定是否已寫入時會不回應、直接關閉連線，Pulse 視為斷線並重連。協商了 `durable` 時這兩種情況的資料都留在本機緩衝、稍後重送（Kairos 去重，不會重複）；沒有協商時（舊版 Kairos 或停用本機緩衝），**該筆 report 視為未送達、不重送**
- **沒有協商 `durable` 時同一時間只有一筆 report 在途**：送出 report 後，到收到伺服器對它（或之後的訊息）的回應（`ack` 或帶 `seq` 的 `error`，含 `storage_unavailable`）之前，新的取樣直接略過（記 `WARN`，節流，附這條連線已略過的次數）；斷線時清除。協商了 `durable` 時最多 4 筆在途（見「本機緩衝」）。兩種情況下伺服器寫檔緩慢時都不會在伺服器端無限積壓
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

以預設值（heartbeat 15 秒）而言，讀取閒置逾時為 `max(30, 15 + 10)` = 30 秒；heartbeat 1 秒時為 11 秒，30 秒時為 60 秒。只有能解析的伺服器訊息（`ack`、`hello_ack`、`error`）才會重置閒置計時，持續送垃圾行的伺服器仍會被判定為閒置。逾時在心跳間隔 `h` 上加 10 秒餘裕，是為了容忍伺服器最多 8 秒的回應延遲：Kairos 等待 report 寫入資料檔期間，同一連線的 heartbeat 也要排隊；Kairos 把寫入逾時（`timeouts.write_secs`，預設 5 秒）的上限定為 6 秒，從收到 report 到回應最壞 0.05（訊息速率）+ 0.5（report 速率限制）+ 6 + 1（寫入寬限）= 7.55 秒 ≤ 8 秒。Pulse 每 `h` 秒內一定會送出 heartbeat，因此最久 `h + 8` 秒內一定會收到伺服器訊息，對任何心跳間隔都小於逾時。逾時不看 `report.interval`：report 依賴取樣任務，取樣卡住時不會送出，不能拿來保證伺服器有回應。

## 回報資料格式

系統資訊以 `report` 訊息送出（一行 JSON，此處為方便閱讀而換行）。協商結果為 v1（舊版 Kairos）時：

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

協商結果為 v2 且雙方都宣告 `metrics` 時（Kairos 0.2.0 以上），改送通用指標。名稱、單位、種類（量測值／計數器）與各平台的來源以 monorepo 的 `crates/kairos-proto/METRICS.md`（第 3 節）為準，以下為摘要（每筆 report 中 CPU、記憶體以外的指標依序為：沒有 label 的 → 掛載點 → 磁碟 I/O 裝置 → 網路介面 → 溫度）：

```json
{
  "type": "report",
  "seq": 3,
  "ts": "2026-10-20T02:00:05.210Z",
  "metrics": [
    {"name": "system.cpu.usage", "value": 17.84, "unit": "%"},
    {"name": "system.memory.total", "value": 51539607552, "unit": "By"},
    {"name": "system.memory.used", "value": 38833356800, "unit": "By"},
    {"name": "system.cpu.load_1m", "value": 2.15, "unit": "1"},
    {"name": "system.memory.available", "value": 12706250752, "unit": "By"},
    {"name": "system.swap.total", "value": 4294967296, "unit": "By"},
    {"name": "system.process.count", "value": 312, "unit": "{processes}"},
    {"name": "system.uptime", "value": 1532941, "unit": "s"},
    {"name": "system.filesystem.used", "value": 265601859584, "unit": "By", "labels": {"fstype": "ext4", "mountpoint": "/"}},
    {"name": "system.filesystem.usage", "value": 55.69, "unit": "%", "labels": {"fstype": "ext4", "mountpoint": "/"}},
    {"name": "system.disk.read", "value": 912345678848, "unit": "By", "labels": {"device": "nvme0n1p2"}},
    {"name": "system.network.packets_received", "value": 87654321, "unit": "{packets}", "labels": {"device": "eth0"}},
    {"name": "system.temperature", "value": 48.5, "unit": "Cel", "labels": {"sensor": "coretemp Package id 0"}}
  ],
  "processes": {
    "top_cpu": [{"pid": 2211, "name": "postgres", "cpu_usage": 9.41, "memory": 4294967296}],
    "top_memory": [{"pid": 1873, "name": "java", "cpu_usage": 3.2, "memory": 8589934592}]
  }
}
```

| 類別 | 指標 | 說明 |
|---|---|---|
| 沒有 label | `system.cpu.usage`、`system.memory.total`／`used`／`available`、`system.cpu.load_1m`／`5m`／`15m`、`system.swap.total`／`used`、`system.process.count`、`system.uptime` | 負載只有 Linux（Windows 不送，不是送 0）。`system.memory.used` = total − available。行程數同 `ps -e` 的列數（不含一般執行緒），`processes.enabled = false` 或只看得到 Pulse 自己的行程（systemd 的 `ProtectProc=invisible`）時不送 |
| 掛載點（label `mountpoint`、`fstype`） | `system.filesystem.total`、`available`、`used`、`usage` | `used` = total − free（free 含保留給 root 的部分，Linux 為 `statvfs` 的 `f_bfree`）；`usage` = used ÷ (used + available)，與 `df` 的 Use% 相同 |
| 磁碟 I/O（label `device`） | `system.disk.read`、`written`；Linux 另有 `read_ops`、`write_ops`、`io_time`（毫秒） | 計數器。Linux 直接讀 `/proc/diskstats`（裝置名稱如 `nvme0n1p2`、`sdb1`、`dm-0`），Windows 為磁碟機代號（`C:`）。讀不到時不送並記一次 `PLS-I8009` |
| 網路介面（label `device`） | `system.network.received`、`transmitted`、`packets_received`、`packets_transmitted`、`errors_received`、`errors_transmitted` | 計數器 |
| 溫度（label `sensor`） | `system.temperature` | 讀不到溫度的感測器略過 |

- **計數器送原始累計值**（開機或介面啟用以來），不在 Pulse 換算速率：取樣遺失或補送時，Kairos 仍能以相鄰兩筆算出正確的平均速率（計數器重置與重新開機的規則見 METRICS.md 第 2.2 節）。記憶體、磁碟與網路以 bytes 為單位（v1 的記憶體為 MB）。
- **篩選**：略過虛擬檔案系統（`tmpfs`、`overlay`、`squashfs` 等）、容量為 0 與同一磁碟的重複掛載點（例如 systemd 沙箱的 bind mount）、迴路與虛擬網路介面（`lo`、`veth*`、`docker*`、`br-*`、`virbr*`、`cni*`、`flannel*`、`cali*`、`vxlan*`、`tunl*`、`kube-*`、`lxcbr*`、`lxdbr*`）與 `collect.interface_exclude`。label 中的控制字元等禁止字元替換成 `?`，長度截到 256 bytes。
- **每類上限與優先順序**：掛載點 16（`/`，Windows 為系統磁碟機，排第一，其餘容量大者優先）、磁碟 I/O 裝置 8（依掛載點的順序）、網路介面 16（有 IPv4 位址者 → 累計流量大者）、溫度感測器 32，合計最多 243 個，不超過協定的 256 個。被略過的項目在第一次取樣時記一則 `INFO`（`sysinfo.snapshot`，附 `skipped_filesystems` 等計數），之後每 24 小時最多一則。
- **單行上限 65536 bytes**：超過時依序捨去，直到不超過：行程的命令列 → 行程排行縮成各 3 項 → 整個行程排行 → 溫度 → 優先順序最低的網路介面、磁碟 I/O 裝置、掛載點（以「整組」捨去，例如一個掛載點的 4 個指標一起捨去）。沒有 label 的指標一律保留。捨去的內容只影響這一筆，debug 日誌註明，每 10 分鐘最多一則 `WARN`。
- 磁碟、網路與溫度在每次取樣時一起刷新（之後會反映新增或移除的掛載點、介面與感測器）；與 CPU、記憶體一樣受取樣逾時保護。

**行程排行**（`processes`，雙方都宣告 feature `processes` 時；Kairos 0.8.0 以上、SQLite 儲存）：每 `processes.interval`（預設 60 秒）的那筆 report 附上依 CPU、依記憶體（常駐記憶體：Linux RSS、Windows Working Set）各前 `processes.top`（預設 5）個行程，其他 report 不帶。`cpu_usage` 是占整台主機的百分比（與 `system.cpu.usage` 同一尺度），期間為上一次排行到這次。行程的詳細資料只在排行的那次取樣讀取，行程數每次取樣只列舉、不讀詳細資料。協商了 `durable` 時隨 report 存入本機緩衝、斷線後一起補送。

- **隱私**：預設只送執行檔名稱（例如 `postgres`，不含路徑與參數）。命令列的參數常含密碼、token、連線字串，**預設不送**；`processes.command = "executable"` 只送執行檔的完整路徑，`"full"` 送完整命令列（含參數，禁止字元換成 `?`、截斷到 1024 bytes），由管理者自行承擔，Kairos 不做遮罩。行程的使用者名稱本版不送。

**主機資產**（`inventory` 訊息，雙方都宣告 feature `inventory` 時；Kairos 0.8.0 以上、SQLite 儲存）：

```json
{"type":"inventory","seq":2,"ts":"2026-10-20T02:00:00.120Z","inventory":{"os":{"family":"linux","name":"Ubuntu","version":"24.04","long_version":"Ubuntu 24.04.1 LTS","kernel":"6.8.0-45-generic","arch":"x86_64"},"boot_time":"2026-10-02T08:11:04Z","cpu":{"model":"Intel(R) Xeon(R) Gold 6338 CPU @ 2.00GHz","vendor":"GenuineIntel","physical_cores":32,"logical_cores":64,"frequency_mhz":2000},"memory":{"total":51539607552,"swap_total":4294967296},"system":{"vendor":"Dell Inc.","product":"PowerEdge R650","serial":"7XK2LM3","board_vendor":"Dell Inc.","board_product":"0PJPW3","bios_vendor":"Dell Inc.","bios_version":"1.12.1"},"disks":[{"mountpoint":"/","device":"nvme0n1p2","fstype":"ext4","total":502468108288,"kind":"ssd","removable":false,"read_only":false}],"network_interfaces":[{"name":"eth0","mac":"52:54:00:12:34:56","addresses":["192.168.10.21/24","fe80::5054:ff:fe12:3456/64"],"mtu":1500}]}}
```

- **何時送**：連線建立（`hello_ack` 之後）收集一次並送出（不論與上次是否相同）；之後每 `inventory.interval`（預設 1 小時）重新收集，內容（`inventory_digest`，不含開機時間）改變才送，距離上次送達滿 24 小時也送一次。Kairos 回 `invalid_report` 時記 `WARN`（`inventory.rejected PLS-W2108`），同樣的內容不再送；回 `storage_unavailable` 時 5 分鐘後重送（Kairos 暫時無法保存記 `PLS-W4101`；資產的速率限制只記 `DEBUG`）。資產不經過本機緩衝（它是狀態，不是時序；重新連線後一定會再送一次）。收集在背景執行緒進行，超過 10 秒記 `WARN`（`PLS-W8005`）、這次不送，不影響 report 的取樣。
- **內容**：取不到的欄位省略（不是空字串或 0）。磁碟與網路介面的篩選、名稱與指標相同，但不受每類上限限制（各最多 64 個）。CPU 時脈為標稱值（型號字串中的「@ 2.00GHz」、Linux 的 `base_frequency`／`cpuinfo_max_freq`、Windows 登錄的 `~MHz`），不用會隨負載變動的目前時脈。BIOS 在 Linux 讀 `/sys/class/dmi/id/bios_*`（所有人可讀），Windows 讀登錄 `HKLM\HARDWARE\DESCRIPTION\System\BIOS`。
- **系統序號**：Linux 的 `/sys/class/dmi/id/product_serial` 只有 root 能讀；發佈包的 `pulse.service` 以 `ExecStartPre=+`（以 root 執行、不受沙箱限制）在啟動前複製到 `/run/pulse/product_serial`，Pulse 依序讀該檔、再直接讀 sysfs（以 root 互動執行時）。都讀不到，或是廠商的預設字串（`To Be Filled By O.E.M.`、`Default string` 等）時不送，記一次 `INFO`（`sysinfo.limited PLS-I8009`）。Windows 由 SMBIOS 讀取，不需要系統管理員權限。`inventory.hardware_serials = false` 不送序號，`inventory.network_addresses = false` 不送 MAC 與 IP。
- 整則超過單行上限時，依序捨去介面的位址（每個介面只留第一個 IPv4 與第一個 IPv6）、再捨去優先順序最低的介面與磁碟。

主機名稱與 `agent_id` 只在連線開頭的 `hello` 送一次，不會重複出現在每筆 report 中。

`cpu_usage` 是「上次取樣到這次取樣之間」（約等於 `report.interval`）所有 CPU 的平均使用率：取樣任務沿用同一個 sysinfo `System`，每次取樣只刷新一次，而不是只量約 200 ms 的瞬間值。第一筆也一樣：取樣任務啟動時先刷新一次作為基準，第一次取樣在一個 `report.interval` 之後，因此不會只量到啟動當下的負載（代價是啟動後要等一個 `report.interval` 才有第一筆 report）。

**取樣逾時**：一次取樣（sysinfo 刷新，Windows 上可能經由 WMI）超過 `clamp(report.interval, 2 秒, 10 秒)` 仍未完成時，記 `WARN` 並略過這次 report。heartbeat 由連線任務獨立送出、不受影響，因此 Kairos 仍會顯示此主機在線，只是不再收到新的 report；本機日誌的 `WARN` 是發現這種情況的依據。卡住的系統呼叫無法中斷：取樣完成前不再發起新的取樣（避免卡住的執行緒越積越多），期間持續略過 report，每 10 分鐘再提醒一次（附已略過的次數）；完成後記 `INFO`「系統資訊取樣已恢復」，以完成當下為新的 CPU 基準，下一次取樣恢復回報。Kairos 端依 `last_report` 停滯時間偵測此情況尚未實作。

Report 為 **at-most-once**：斷線期間的取樣、上一筆 report 尚未收到回應時的取樣、以及伺服器回 `storage_unavailable` 的那一筆，都會直接丟棄，不補送。

## 重連行為

- 任何錯誤（連線失敗、TLS 失敗、逾時、伺服器關閉連線、收到 fatal error、註冊前收到任何 error）都會關閉目前連線，等待後重連。
- 等待時間從 `retry.initial_delay` 秒開始，每次加倍，最多 `retry.max_delay` 秒，並加上 ±20% 隨機抖動，避免大量 agent 同時重連。到達上限後，延遲在 `[0.8 × max_delay, max_delay]` 間均勻分佈（不會全部集中在上限值）。
- **啟動與第一次重連的隨機延遲**：啟動時先等 0～`min(heartbeat.interval, 5)` 秒（均勻分佈，以 `INFO` 記錄「x 秒後開始連線」）才第一次連線；斷線後的第一次重連（連續失敗第 1 次）在上述退避之外另加 0～`min(heartbeat.interval, 5)` 秒。Kairos 重新啟動時所有 agent 同時斷線，只靠初始退避（2 秒 ±20%）會在約 1 秒內一起湧入；加上分散後攤在約 1.6～7.4 秒（預設值）。上限取心跳間隔，分散不超過一個心跳週期，不影響伺服器的離線判定；以 5 秒封頂，避免心跳間隔很長時拖慢啟動與恢復。之後的重試已由指數退避與抖動錯開，不再另加。
- **不限重試次數**，程式不會因伺服器暫時不在而結束。
- 每次斷線只記**一則**日誌，內容包含斷線原因、處置提示（若有）、本次連線維持多久與退避是否重置，以及下次重連的等待時間。連續失敗未達 `retry.max_retries` 次時以 `WARN` 記錄，達到後改用 `ERROR`，但同樣節流：伺服器長時間停機時每次重試都會失敗，`ERROR` 每 10 分鐘最多一則，期間其他次數改記 `WARN`，下一則 `ERROR` 會註明「上次以 ERROR 記錄後，另有 n 次連線失敗已改記 WARN」（連線穩定 30 秒以上、退避重置後重新起算）；這類一般連線錯誤不寫 stderr；伺服器送來的 fatal error、註冊前收到的 error 與協定版本不符以 `ERROR` 記錄（`superseded` 例外，見下）。這些錯誤每次重連都會重複，因此依錯誤碼節流：同一錯誤碼每 10 分鐘最多一則 `ERROR`，期間其他次數改記 `WARN`，下一則 `ERROR` 會註明「上次以 ERROR 記錄後，另有 n 次相同原因的斷線已改記 WARN」；錯誤碼改變時立即以 `ERROR` 記錄。
- **設定了多個伺服器位址**（`server.fallback_addresses`）時，上面的退避以「一輪」為單位：收到 `hello_ack` 之前失敗就立即改試下一個位址，整輪都失敗才退避；Kairos 排空（`drain`）造成的斷線不退避。見下一節。
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

## 多個伺服器位址與排空（0.9.0）

Kairos 的高可用部署（`kairos/HA.md`）有多個節點。**前面有 L4 負載平衡器或虛擬 IP（keepalived）時 Pulse 不需要任何修改**：`server.address` 指向負載平衡器或虛擬 IP 的名稱即可。沒有負載平衡器、或負載平衡器本身也要備援（例如兩個機房各一個）時，設定 `server.fallback_addresses`：

```toml
[server]
address = "kairos-a.example.com"                         # 第一優先
port = 8443
domain = "kairos.example.com"                            # 所有位址共用的憑證驗證名稱（不變）
fallback_addresses = ["kairos-b.example.com", "10.0.20.11:8443"]
```

- **格式**：每一項為「主機」或「主機:埠」，省略埠時用 `server.port`；IPv6 必須加方括號（`"[2001:db8::10]"`、`"[2001:db8::10]:8443"`）。最多 16 個，重複的只留第一個。不合法時無法啟動（`PLS-E0201`，`pulse check-config` 也會檢查）。環境變數為 `PULSE__SERVER__FALLBACK_ADDRESSES`，以逗號分隔（去除前後空白，空字串表示沒有）。
- **新的鍵、不改 `address`**：舊版 Pulse 忽略不認得的鍵，降版後照常連 `address`；`pulse enroll` 改寫設定檔時也只寫 `address`。**安裝字串不變**：叢集部署的安裝字串一律使用服務名稱（負載平衡器或虛擬 IP），多位址在註冊後以 `pulse.toml` 或環境變數加入。
- **憑證**：所有位址都以同一個 `server.domain` 驗證伺服器憑證（與實際連上的位址無關），因此**每個節點的伺服器憑證都必須含這個名稱**；某個節點的憑證不含時，該節點記 `PLS-E1402` 並改試下一個。
- **選擇與退避**：每一輪從第一個位址（`address`）開始依序嘗試。收到 `hello_ack` 之前的任何失敗（DNS、TCP 被拒、逾時、TLS 握手失敗、收到錯誤或斷線）都**立即**改試下一個（兩次之間只隔 0～500 ms 的隨機延遲），不增加退避，記 `PLS-W2301`（每輪第一次失敗時判斷，`WARN` 每 10 分鐘最多一則，其餘記 `DEBUG`）。整輪都失敗才依 `[retry]` 退避，記 `PLS-E2302`，日誌列出每個位址最後的錯誤；某個位址的錯誤需要人工處理（例如伺服器憑證不符、用戶端憑證被拒）時改以該錯誤的代碼記錄。名稱解析出多個 IP 時依序嘗試，每個 IP 最多 3 秒，整個位址最多 5 秒。
- **連上之後不主動切回第一優先**；斷線後的下一輪自然從第一個開始（避免第一優先節點反覆故障時來回切換）。被 `superseded` 時不改試其他位址（身分衝突，換節點也一樣）。
- **排空（feature `cluster`）**：Pulse 0.9.0 一律宣告 `cluster`；Kairos 0.9.0 列出時，`hello_ack` 帶回應的節點名稱（`server_node`），Kairos 優雅關機時先送 `drain`。Pulse 收到後停止送出新的 report（取樣照常寫進本機緩衝）、等在途的 report 收到回應（最多 8 秒）後關閉連線，在 `[0, reconnect_within_ms]` 的隨機延遲後從**目前位址的下一個**開始重連。這次斷線不算失敗、不增加退避，記 `PLS-I8507`（`INFO`）。
- **補送與去重**：重送的 durable report 內容不變（`boot_id`、`report_seq` 與連到哪個節點無關），斷線時在途的 report（最多 4 筆）在下一條連線（可能是另一個節點）重送。**所有位址必須屬於共用同一個資料庫的同一個叢集**，去重才是全叢集的；指到另一套獨立的 Kairos 時，換節點時在途的那幾筆可能兩邊各寫一次。`hello_ack` 的 `cluster_id`（叢集模式的 Kairos 才送）在同一個 Pulse 行程內改變時記 `WARN`。
- **憑證更新**：8443 的正常輪替（`renew_request`）在任何節點都能完成；以新憑證確認的一次性連線、除役通知與過期憑證的報到都從最近一次連上的位址開始依序嘗試。8444（註冊、輪詢、過期續簽）仍只用 `enroll.address`（空字串時為 `server.address` 的主機），見「目前限制」。
- **狀態**：`pulse status` 顯示「已註冊到 kairos-b.example.com:8443（Kairos 節點 kairos-b；第 2 個位址，共 3 個）」；所有位址都失敗時的說明列出每個位址最後的錯誤。`status.json` 的 `connection.server` 為實際連上的位址，另有 `server_node`、`candidates`、`candidate_index`、`candidate_errors`。`pulse check-config` 的「伺服器」列出備用位址；`pulse diag` 的 `candidates.json` 逐一檢查名稱解析與 TCP 連線。本機指標見「本機指標」。
- DNS SRV（`server.srv`）是 HA 規格中階段 B 的選用功能，**這個版本沒有實作**。

## 集中設定與遠端升級（0.9.0）

規格見 `crates/kairos-proto/REMOTE.md`（第 3、4 節）與 PROTOCOL.md「遠端管理」。兩者都只在 Kairos 0.9.0 以上、以 SQLite 儲存、mTLS（用戶端憑證）的連線上協商（feature `config`、`upgrade`）；連舊版 Kairos 時與 0.8 完全相同。hello 另帶 `install`（安裝方式、遠端升級與集中設定的本機狀態），讓主控台區分「Pulse 太舊」「本機停用」「缺少輔助程式」等原因。

### 集中設定（`[remote_config]`）

- **可下發的鍵**（白名單，REMOTE.md 第 3.1 節）：`heartbeat.interval`、`report.interval`、`retry.*`、`[collect]` 的開關與上限、`collect.interface_exclude`、`processes.enabled`／`interval`／`top`／`command`、`inventory.*`、`buffer.enabled` 與上限、`log.level`、`log.retention_days`。**連線位址、憑證、身分、路徑、syslog、`[remote_config]`、`[upgrade]` 永遠只能在本機改**，Kairos 送來也只會列在 `config_status` 的 `ignored`。
- **合併順序**（後者優先）：內建預設值 < Kairos 下發 < `pulse.toml` < `PULSE__*` 環境變數。本機明確寫出的鍵永遠優先，回報為「本機覆蓋」（`PLS-I0503`）；要交給 Kairos 管理的鍵請從 `pulse.toml` 移除。例外：`collect.interface_exclude` 與本機（或內建預設）的清單取聯集；`processes.command`、`inventory.hardware_serials`、`inventory.network_addresses` 只接受比本機（或內建預設）**更保守或相同**的值，否則忽略（`not_more_restrictive`）。
- **驗證**：合併後以與啟動時相同的規則檢查；任何一個鍵不合法（型別、範圍、互相矛盾）時**整份拒絕**、沿用上一版（`config.rejected PLS-W0501`），`config_status` 的 `errors` 列出原因。`hash` 與內容不符的 `config` 當作沒有收到。
- **套用**：即時的鍵就地生效、不中斷連線——`log.level` 立即；`report.interval`、`collect.*`、`processes.interval`／`top`／`command` 以新的設定重新開始取樣；`inventory.interval` 與序號、位址的開關在下一次收集；`buffer.max_*` 在下一次寫入；`retry.*`、`buffer.replay_interval_ms` 與 hello 的 `report_interval_ms` 在下一次連線。需要重新連線的鍵（`heartbeat.interval`、`processes.enabled`、`inventory.enabled`、`buffer.enabled`）在送出 `config_status` 後結束目前的連線，以新的設定重建取樣、本機緩衝與連線（durable 的積壓照常補送）。沒有任何下發的鍵需要重新啟動 Pulse。日誌 `config.applied` 列出改變的鍵與新舊值。
- **快取與離線**：套用的版本寫在狀態目錄的 `remote/config.json`（Linux 為 `STATE_DIRECTORY`，預設 `/var/lib/pulse`；Windows 為 `%ProgramData%\Pulse`，環境變數 `STATE_DIRECTORY` 設為絕對路徑時改用它；目錄 0750、檔案 0640，寫暫存檔再改名，存放 `rev`、`hash` 與內容），Kairos 停機或重新啟動 Pulse 後沿用，沒有期限；Kairos 恢復後版本相同時不再取得，只回報一次。快取損毀時忽略、改用本機設定；無法寫入時不套用新的版本（`config.cache_unavailable PLS-E0506`）。連上不支援集中設定的 Kairos（舊版、JSONL 儲存）時快取保留但不使用。
- **確認期與回復**：即時套用的版本在 `max(3 × heartbeat.interval, 60 秒)` 內沒有斷線、需要重新連線的版本在重連後收到 `hello_ack`，即標為 last-known-good。確認前連續 3 次無法完成註冊時回復上一份可用的設定（`config.rolled_back PLS-W0504`），同一版不再自動套用，Kairos 下發新版才再試；套用後尚未確認就連續兩次重新啟動（崩潰）也同樣回復。
- **停用**：`[remote_config] enabled = false` 時不使用快取、只用本機設定，停用後第一次連線通知 Kairos 一次（`state: disabled`、`PLS-I0505`），之後不再宣告 `config`。
- **檢視**：`pulse config show [--effective] [--sources] [--json]` 列出白名單鍵的值（`--effective` 合併快取；`--sources` 標出 `default`／`remote:<設定檔>`／`pulse.toml`／`env`）。`pulse check-config --with-remote` 連同快取一起檢查。`pulse status` 的「設定」列附上「集中設定：已同步（第 42 版，2 個鍵被本機覆蓋）」等狀態，`--json` 另有 `management.config`。

### 遠端升級（`[upgrade]`，Linux）

- **前提**：Linux 的 tar（`/opt/pulse`）、deb 或 rpm 安裝，以 systemd 管理的單一實例（多實例、容器、`unknown` 安裝方式不支援，主控台顯示原因）；**信任錨** `/etc/kairos-pulse/allowed_signers`（`ssh-keygen` 的格式，只接受 `ssh-ed25519` 與 `namespaces="kairos-pulse-release"`，必須由 root 擁有、其他人不可寫；不存在或權限過寬時 `upgrade.trust_anchor_unavailable PLS-E8403`）；**輔助程式** `pulse-upgrade.path` 已啟用（套件會安裝並啟用，tar 安裝見 INSTALL.md 第 10.1 節）。**安裝路徑**（0.10.0）：`/opt/pulse`、`/opt/pulse/pulse`、`/var/lib/pulse-upgrade` 與它們的每一層上層目錄必須屬於 root、群組與其他人不可寫，`/opt/pulse` 不得是符號連結（依 INSTALL 或套件安裝的主機符合）；不符合時 Pulse 在 hello 回報 `install.upgrade = "unsafe_path"`、主控台的資格預覽標出這台主機，`pulse upgrade status` 的「原因」寫出是哪一個路徑（與升級輔助程式用的是同一套檢查；服務帳號看不到的部分略過，由輔助程式在執行時檢查，不通過時 `PLS-E8405`）。條件都符合時 `install.upgrade = "ready"` 才宣告 `upgrade`。每台主機都要先**手動升級一次到 0.9.0**。
- **流程**：Kairos 在心跳的回應中提示 → `upgrade_get` 取得提議 → 本機檢查（停用、外部 URL、平台與格式、暫存空間 `3 × 發佈檔大小 + 64 MiB`）→ 以本機信任錨驗證 `SHA256SUMS.sig`、清單與發佈檔名 → 版本政策（降版需要推送與本機 `allow_downgrade` 都允許，`PLS-W8410`；低於清單的 `min_upgrade_from` 時拒絕）→ 經同一條 8443 連線分段下載（每段 32 KiB、最多 2 段在途、斷線或重新啟動後續傳、`rate_limited` 時依 `retry_after_ms` 等待；SHA-256 連續 3 次不符時放棄，`PLS-E8402`）→ 暫存在 `/var/lib/pulse/upgrade/<upgrade_id>/` → 進入升級時段時先向 Kairos 再確認提議（推送已暫停或取消時不替換），再送 `applying`、寫入 `request.json`。
- **替換**（root 的 `pulse-upgrade.service`，以目前已安裝的執行檔執行 `pulse upgrade apply`，不連網路）：把請求與暫存的檔案複製到 root 專用的 `/var/lib/pulse-upgrade/<upgrade_id>/`（拒絕符號連結、硬連結與超大檔案），以 root 擁有的 `pulse.toml` 中的 `[upgrade]` 重新驗證，預檢新版（以 `pulse` 帳號執行 `pulse --version` 與 `pulse check-config --json --with-remote`），保留 `pulse.prev` 後替換（tar：改名換上，`pulse.service` 沒被改過時一併更新；deb：`dpkg -i --force-confold`；rpm：`rpm -U --oldpackage --replacepkgs`；套件資料庫被鎖住時每 30 秒重試、最多 10 分鐘），`systemctl restart pulse`。
- **健康確認與回滾**：期限為 Kairos 推送的 `health_timeout_secs`（本機 `upgrade.health_timeout_secs` 非 0 時取兩者較大）。服務 `active`、`NRestarts` 沒有增加、狀態檔由新的行程（PID 與版本相符）寫出、已註冊且有 report 被 Kairos 確認，再穩定 60 秒即成功（`upgrade.completed PLS-I8416`）。逾時、崩潰重啟或以結束代碼 2／3／4 停止時回滾：tar 換回 `pulse.prev`（與 unit）；deb／rpm 以提議附的回滾檔裝回（只在本機有它所屬發佈的驗證紀錄時；否則換回 `pulse.prev`，`detail` 註明套件資料庫仍為新版），舊版恢復即 `upgrade.rolled_back PLS-W8407`，回滾後也不健康時 `upgrade.rollback_failed PLS-E8408`（保留所有暫存與紀錄，不再動作，需要人工處理；`pulse status` 結束代碼 9）。主機在升級的任何一步重新開機、斷電或輔助程式被終止時，輔助程式依 root 的交易紀錄（`/var/lib/pulse-upgrade/active.json` 指向的 `<id>/state.json`，有 checksum 與驗證過的檔案的雜湊）與磁碟上的實況接續：還沒替換完就繼續替換、已替換就重新確認健康（期限已過時回滾）、回滾到一半就重做還原；接續時只用 root 工作目錄裡驗證過的複本，`request.json` 被改寫或換掉都不影響，`pulse.prev` 不會被新版覆寫（REMOTE.md 第 17.9 節）。交易紀錄或指標損毀、遺失時輔助程式不替換也不回滾（`PLS-E8402`，`state_unusable`／`transaction_pointer_unusable`）：檢查主機後刪除 `/var/lib/pulse-upgrade/active.json` 與對應的工作目錄，才能再遠端升級。`/opt/pulse`、`/var/lib/pulse-upgrade` 或它們的上層目錄不屬於 root、可由群組或其他人寫入時，輔助程式拒絕執行（`PLS-E8405`，`unsafe_path`）。
- **回報**：`upgrade_status`（`accepted`、每 25% 的進度、`staged`、`applying`，以及替換後的 Pulse 讀 `result.json` 回報的 `succeeded`／`failed`／`rolled_back`／`rollback_failed`）收到 `ack` 才算回報，斷線後重送。
- **檢視與操作**：`pulse upgrade status [--json]`（安裝方式、是否就緒與原因、信任錨的金鑰指紋、進行中的升級、最近一次結果）；`sudo pulse upgrade rollback --yes`（tar 安裝手動換回 `pulse.prev` 並重新啟動；deb／rpm 顯示套件管理員的回滾方式）。紀錄：`journalctl -u pulse-upgrade` 與 `/var/lib/pulse-upgrade/<id>/upgrade.log`（`pulse diag` 一併收集，連同集中設定的快取與升級的狀態檔）。`pulse status` 有進行中的升級或最近 24 小時的結果時另有「升級」列。
- **停用**：`pulse.toml` 的 `[upgrade] enabled = false`（`PLS-I8409`；環境變數 `PULSE__UPGRADE__ENABLED=false` 也能關閉，但不能以環境變數開啟），或 `systemctl disable --now pulse-upgrade.path`。一行安裝腳本有 `--no-remote-upgrade`、`--no-remote-config`。
- **安全模型**：Pulse 只安裝本機信任錨驗證過的官方發佈包；Kairos 被入侵時最多只能推送另一個官方簽章的版本，且預設不能降版。集中設定與遠端升級都不能修改信任錨。

## 本機緩衝（at-least-once）

協商了 `durable`（Pulse 與 Kairos 都是 0.3.0 以上、`buffer.enabled = true`）時，report 改為 at-least-once（至少送達一次）。規格見 PROTOCOL.md「durable：at-least-once 投遞」。

- **流程**：每次取樣先寫進 `buffer.dir`（預設 `buffer/`），連線 task 依序送出，收到 Kairos 的 `ack` 才從緩衝刪除。斷線（含 Kairos 停機、重新啟動、寫入結果不明而斷線）期間的取樣留在緩衝中；重新連上後先依序補送積壓（含斷線前已送出、還沒收到 `ack` 的），再送新的取樣。重送的資料與第一次送出的相同（`boot_id`、`report_seq`、`ts` 與內容都不變），Kairos 以 (agent_id, `boot_id`, `report_seq`) 去重，已保存過的回 `ack` 但不再寫入。
- **`boot_id` 與 `report_seq`**：`boot_id` 是 Pulse **每次程序啟動**時產生的 UUID v4（啟動日誌「本機緩衝：…本次 boot_id …」），`report_seq` 從 1 開始、每筆取樣加 1。重新啟動前留在緩衝中的資料沿用原本的 `boot_id` 與 `report_seq`。
- **補送速率**：兩筆 report 之間至少間隔 `buffer.replay_interval_ms`（預設 1200 毫秒，每秒約 0.83 筆，低於 Kairos 的 report 速率限制每秒 2 則），同一時間最多 4 筆在途。實際使用的間隔不超過 `report.interval` 的 60%（`report.interval = 1` 時為 600 毫秒），否則新的取樣比送出的快、積壓補不完；啟動日誌「本機緩衝：…補送間隔 N 毫秒…」為實際的值。有積壓時，開始補送前另加 0～`min(heartbeat.interval, 5)` 秒的隨機延遲（與重連的分散相同）。以預設值，積壓每秒約減少 0.63 筆（新的取樣同時以每 5 秒一筆加入）：斷線 1 小時（720 筆）約 19 分鐘補完、24 小時（約 17,000 筆）約 7.6 小時補完。預設值依 1 萬台同時恢復連線時 Kairos 的寫入量（約每秒 8,300 筆）決定，見 Kairos 的 STORAGE.md 第 11.1 節；主機少、希望補得快時可調小（500 時 24 小時的積壓約 2.7 小時補完）。
- **Kairos 回應**：`ack` → 刪除；`storage_unavailable`（Kairos 磁碟已滿、每日大小上限、寫入逾時、速率限制）→ 保留，退避 2 秒起、加倍到 60 秒後從最舊的未確認資料重送；`invalid_report`、`bad_request`（這筆永遠不會被接受）→ 丟棄並記 `WARN`。
- **容量**：待送資料超過 `buffer.max_mb`（預設 128 MiB；0.7 以前為 64）或 `buffer.max_records`（預設 50,000 筆）時丟掉最舊的資料並記 `WARN`（節流，附累計丟棄筆數）；取樣時間超過 `buffer.max_age_hours`（預設 7 天）的資料不再送出。0.8.0 起每筆約 1.5～5 KB（帶行程排行的那筆再多約 1 KB），預設每 5 秒一筆時約可保存 2～3 天（與 `max_records` 的約 69 小時相近，先到者為準）。磁碟用量最多約 `max_mb` 再加 8 MiB。0.7 寫入的緩衝記錄（沒有行程排行）升級後仍可讀取、照常補送。
- **斷電與效能的取捨**：寫入不逐筆 fsync，最多每 10 秒（以及換區段檔與正常結束時）fsync 一次，因此斷電時最多遺失最近約 10 秒的取樣（預設約 2 筆）；每筆記錄有長度與 CRC-32，啟動時偵測並截掉寫到一半的尾端（記 `WARN`）。已送達的位置（`spool.ack`）最多每 5 秒更新一次，斷電後可能有幾筆已送達的資料再送一次，由 Kairos 去重。
- **檔案**：`buffer/spool-<區段編號>.dat`（區段檔，`max_mb / 16`、64 KiB～4 MiB，全部送達後刪除）與 `buffer/spool.ack`（送達位置）。Unix 上目錄以 0750、檔案以 0640 建立；目錄允許其他使用者存取時啟動記 `WARN`。Windows 上檔案繼承目錄的 ACL，目錄允許 Users、Everyone 等一般使用者存取時記 `WARN`，建議以 `icacls "<目錄>" /inheritance:r /grant:r "<執行帳號>:(OI)(CI)M" "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F"` 只授權給執行帳號、SYSTEM 與 Administrators。**一個緩衝目錄只能給一個 Pulse 使用**（多實例時各自一個，見 INSTALL.md 第 9 節）。
- **舊版 Kairos**（0.2.0 以前，不宣告 `durable`）：連線期間的取樣照舊即時送出（at-most-once，不寫入緩衝）；斷線期間寫進緩衝的積壓不會送給舊版 Kairos，保留到連上 0.3.0 以上的 Kairos，或超過 `max_age_hours`、容量上限而被丟棄。
- **無法開啟緩衝**（目錄無法建立或寫入，例如沿用舊版 systemd unit 而 `buffer/` 不可寫）：記 `ERROR`（同時寫 stderr），本次執行改為 at-most-once，不影響監控本身。
- `buffer.enabled = false`：不寫磁碟、不宣告 `durable`，行為與 0.2.0 相同。

## 本機狀態（pulse status）

依設計包 `pulse/cli-spec.md` §4。`pulse status` **不連線、不影響服務**：只讀取執行中的 Pulse 寫出的本機狀態檔，並向 systemd／Windows 服務控制管理員查詢服務狀態。

```
$ /opt/pulse/pulse status
Pulse 0.5.0 (b81e2d4) · plc-hmi-07 · agent_id 9422cb70-1b2c-4d3e-9f00-112233445566

✓ 通過 服務   pulse.service 執行中（PID 4151145，已執行 12 天 4 小時）
✓ 通過 連線   已註冊到 kairos.example.gov.tw:8443（TLS 1.3，mTLS），已連線 2 分 2 秒
⚠ 警告 回報   最後回報 2 秒前
              補送中：待補送 49,924 筆（最舊 2026-10-06 17:14:40 UTC+8，2 天 21 小時前），
              預計約 21 小時 54 分補完
              本機緩衝曾滿：已丟掉最舊的 3,676 筆（2026-10-06 12:00～17:06 的取樣，PLS-W4202）
✓ 通過 身分   憑證有效至 2027-10-09（剩餘 365 天）
✓ 通過 設定   /opt/pulse/pulse.toml，無警告
✓ 通過 時鐘   與 Kairos 相差 0.2 秒

本機狀態：警告
結果：1 個警告、5 項通過。
```

檢查列寫 stdout，有問題時另在 stderr 印出三段式說明（標題、原因、處理方式與代碼）；`pulse status > status.txt` 只會得到檢查列。非 TTY（導向檔案、管線、部署工具）時改用 ASCII 標籤（`[ OK ]`、`[WARN]`、`[FAIL]`、`[DOWN]`、`[SKIP]`）且不上色；`--ascii`、`PULSE_SYMBOLS=ascii`、`--color never`、`NO_COLOR` 也可指定。

| 選項 | 說明 |
|---|---|
| `--brief` | 單行，第一欄為本機狀態：`offline   plc-hmi-07 reconnecting 12m30s PLS-E2003 backlog=149` |
| `--json` | 機器可讀（schema `pulse.status/v1`，純 ASCII，非 ASCII 字元以 `\uXXXX` 跳脫）；欄位見設計包 `cli-spec.md` §4.8，腳本請讀 `data.status`、`data.checks[]`、`data.issues[]` |
| `--watch [秒]` | 每 N 秒（預設 2）重新顯示，按 Ctrl+C 結束 |
| `--exit-codes=nagios` | 單行輸出，結束代碼為 Nagios 慣例（0 OK、1 WARNING、2 CRITICAL、3 UNKNOWN；沒有權限讀取狀態檔時也是 3） |
| `-c, --config <路徑>` | 指定 `pulse.toml`；狀態檔等相對路徑以它所在的目錄解析 |
| `--instance <N>` | 多實例 N：讀 `/var/lib/pulse/N/status.json`，查詢 `pulse-N.service` |
| `-q`／`-v` | 不顯示警告等級的說明／說明中加上技術細節 |

**本機狀態與結束代碼**（`cli-spec.md` §1.2、§1.3）：

| 本機狀態 | 判定 | 結束代碼 |
|---|---|---|
| `online` 線上 | 已註冊、連線中、沒有警告 | 0 |
| `warning` 警告 | 連線中但有警告：憑證 14 天內到期、時鐘相差 30 秒以上、設定或權限警告、最近 24 小時內本機緩衝丟過資料（`PLS-W4202`／`PLS-W4203`）、最近 1 小時內無法寫入緩衝（`PLS-W4103`）、緩衝無法使用（`PLS-E4004`）、Kairos 暫時無法保存（`PLS-W4101`） | 8 |
| `offline` 離線 | 一般網路錯誤、中斷未滿 30 分鐘（Pulse 會自行重連） | 9 |
| `critical` 嚴重 | 需要人工處理（憑證被撤銷、驗證失敗等）、中斷滿 30 分鐘、服務未執行或未安裝、時鐘相差超過 5 分鐘、設定檔有誤 | 9 |
| `unknown` 未知 | 服務在執行，但狀態檔超過 60 秒沒有更新、不存在或無法解析（`PLS-W8004`，Pulse 可能卡住）。連線、回報、身分、時鐘列以最後已知內容顯示並標示「（最後已知，N 分鐘前）」，沒有內容時略過 | 9 |
| — | 用法錯誤／沒有權限讀取狀態檔（`PLS-E8203`） | 2／10 |

結束代碼 9 涵蓋三種狀態，要區分短暫重連與需要人工處理時請讀 `--json` 的 `data.status`（SCCM 的判讀方式見設計包 `pulse/sccm-deployment.md`）。

**「回報」列的補送資訊**：連線中有積壓時寫「補送中：待補送 N 筆（最舊…），預計約…補完」（預計時間＝待補送筆數 ÷（1000 ÷ 實際補送間隔 − 1 ÷ `report.interval`））；中斷中寫待補送筆數與緩衝用量（達 80% 時提醒）；不補送時（停用緩衝、緩衝無法使用、連上 0.2 以前的 Kairos）寫原因。

**設定檔與狀態檔的位置**：`pulse status` 依序找 `--config`、目前目錄的 `pulse.toml`、`pulse` 執行檔所在目錄的 `pulse.toml`（發佈包的佈局是兩者都在 `/opt/pulse`，因此在任何目錄執行 `/opt/pulse/pulse status` 都可以；一般帳號讀不到 640 的 `pulse.toml` 時使用預設值）。狀態檔為 `status.file`，未設定時為設計包的位置：Linux `/var/lib/pulse/status.json`（多實例 N 為 `/var/lib/pulse/N/status.json`，加 `--instance N`），Windows `%ProgramData%\Pulse\status.json`；另外也讀工作目錄的 `logs/status.json`（沿用舊 unit 時的備用位置），兩者都有時取較新的。設定檔有誤時仍以預設位置讀取狀態檔，並在「設定」列顯示錯誤（`PLS-E0101`／`PLS-E0201`）。

**服務狀態**：Linux 以 `systemctl show`（一般帳號可執行，最多等 5 秒）查詢 Pulse 寫在狀態檔中的 unit（以 systemd 啟動時自動偵測，例如 `pulse-2.service`，沒有時為 `pulse.service`）；Windows 查詢服務 `Pulse`。找不到服務時（前景執行、容器、服務包裝工具使用其他名稱），以狀態檔記錄的 PID 是否還在判斷（「未由服務管理員管理」）。「最近 1 小時重新啟動 ≥ 3 次」依狀態檔記錄的啟動時間計算。

**本機狀態檔**（`status_file.rs`、`local_status.rs`）：

- 內容：版本、PID、啟動時間、設定檔、連線（協商的協定版本與 features、最近的錯誤代碼、上次被拒的原因、上一次中斷的期間）、回報（最後回報與確認時間、最近 1 小時的取樣／確認／拒收筆數、投遞方式）、本機緩衝（待補送筆數與 bytes、最舊一筆的取樣時間、上限與用量、累計丟棄筆數與被丟掉的取樣時間範圍、`last_dropped_at`、寫入失敗）、用戶端憑證（路徑、序號、有效期間）、時鐘偏差、設定與權限警告。**不含秘密**：沒有私鑰（也不寫私鑰路徑）、token 或未清理的伺服器文字；未啟用 mTLS 時 `agent_id` 等同身分憑據，不寫入，其他欄位中出現的 agent_id 換成 `<agent_id>`（`agent_id_hidden: true`；`pulse status` 由 root 執行時從 `agent.id` 讀取）。schema `pulse.status-file/v1`，只新增欄位。
- 寫入：每 10 秒一次，連線狀態、錯誤代碼、緩衝狀態改變或丟棄資料時另外寫（兩次至少間隔 1 秒；補送中待補送筆數每秒都在變，不另外寫）。以同目錄的暫存檔（`create_new`、隨機檔名）寫入後 rename 取代，讀取端只會看到完整的舊檔或新檔。正常結束前寫最後一份（`running: false` 與結束代碼）。
- 磁碟負擔的取捨：一份約 3～5 KB，每 10 秒一次約 0.4 KB/s（一天約 40 MB 的寫入量）；**不 fsync**，大部分寫入在作業系統的快取中合併。狀態檔只描述「現在」，斷電後下一次寫入（10 秒內）就會更新；少數檔案系統斷電後可能留下空檔，`pulse status` 視為無法解析（未知），服務啟動 10 秒內恢復。
- 過時判斷：寫入時記錄單調時鐘（Linux `/proc/uptime`、Windows `GetTickCount64`），`pulse status` 以同一個時鐘相減，不受系統時間調整或與 Kairos 的時鐘偏差影響；重新開機後一律視為過時。
- 位置與權限（`cli-spec.md` §4.1）：**所有人可讀，`pulse status` 不需要 root**。Linux 為 `/var/lib/pulse/status.json`，由 `pulse.service` 的 `StateDirectory=pulse`（`StateDirectoryMode=0755`）在 `ProtectSystem=strict` 下建立並開放寫入；以 systemd 執行時優先使用 `STATE_DIRECTORY`（多實例 `StateDirectory=pulse/N`）。檔案 0644、Pulse 自行建立的目錄 0755，以 `set_permissions` 明確設定（unit 的 `UMask=0077` 不影響）。發佈包的 `/opt/pulse` 為 751，一般帳號可以執行 `/opt/pulse/pulse`。Windows 為 `%ProgramData%\Pulse\status.json`，Pulse 在檔案上另外授予 BUILTIN\Users 讀取（目錄被收緊時仍可讀）。沒有權限讀取時結束代碼 10。
- 預設位置無法寫入時（例如沿用沒有 `StateDirectory=` 的舊 unit）：**服務照常啟動與監控**，狀態檔改寫在工作目錄的 `logs/status.json`，記一次 `WARN`（`status.write_failed PLS-E8204`），`pulse status` 的「設定」列為警告（只有 root／`pulse` 群組讀得到 `logs/`）。預設位置恢復可寫後刪除 `logs/` 中的舊檔。執行中無法寫入狀態檔時，第一次記 `WARN`（`status.write_failed PLS-E8204`），連續失敗滿 60 秒（`pulse status` 開始顯示「未知」）記 `ERROR` 並寫 stderr，之後每 10 分鐘一則 `ERROR` 摘要；恢復時記 `INFO`（`status.write_restored`）。

## agent.id

- 第一次啟動時產生一個 UUID v4 並寫入 `agent.id`（路徑由 `agent.id_file` 設定），之後每次啟動讀檔沿用。
- 檔案內容必須是 **UUID 文字格式**：`8-4-4-4-12` 個十六進位字元，共 36 字元，大小寫皆可（例如 `6f1c0e4a-1b2c-4d3e-9f00-112233445566`）。讀檔時會先去除 UTF-8 BOM（Windows 記事本存檔可能加上）與前後空白。
- 識別**不分大小寫**：讀入後一律轉成小寫再使用（hello、日誌、`--print-agent-id`、客戶端憑證 SAN 檢查），與伺服器的正規化一致；檔案本身不會被改寫。
- 檔案超過 **1024 bytes**（例如 `agent.id_file` 誤指到其他大檔案）時不會整個讀進記憶體，而是啟動即報錯；內容不是 UTF-8 文字時也明確報錯。
- 檔案已存在時只讀取、不寫回（不改寫大小寫、不調整權限），因此可以設為服務帳號唯讀（發佈包的 INSTALL 使用 `root:pulse 640`）。
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
├── cli.rs             命令列參數解析（status、explain、enroll、decommission、--print-agent-id、--version、--help）與版本字串
├── codes.rs           日誌事件（log-ux §4，含 Windows 事件識別碼）與錯誤代碼
├── status_file.rs     本機狀態檔的格式、原子寫入、讀取與單調時鐘
├── local_status.rs    執行中記錄狀態並定期寫出狀態檔
├── status_cmd/        pulse status：判定（eval）、三段式說明（issues）、輸出（render、json）、
│                      服務管理員查詢（service）、色彩與符號（term）、時間與數字格式（fmt）、golden 測試
├── config_manager.rs  設定管理（嵌入預設 → pulse.toml → PULSE__ 環境變數）與範圍驗證
├── file_perms.rs      敏感檔案（agent.id、client_key）與本機緩衝目錄的權限檢查（Unix 權限位元、Windows ACL）
├── tls.rs             rustls 設定：信任庫、客戶端憑證、SAN 與有效期限檢查、TLS 錯誤提示
├── protocol.rs        協定訊息定義（ClientMsg / ServerMsg）
├── network.rs         長 TLS 連線：hello / heartbeat / report / 逾時 / 重連 / 日誌清理；durable 時的補送與重送
├── spool.rs           本機緩衝（at-least-once）：區段檔、CRC 與尾端截斷、容量上限、收集 task 的寫入
├── enroll/            註冊通道 8444 用戶端：pulse enroll（cli）、信任建立（trust）、HTTP/1.1（http、client）、
│                      私鑰與 CSR（keys）、identity/ 與 enrollment.json（store）、排程與狀態機（schedule、machine）、
│                      流程（flow）、服務端閘門與背景輪詢（service）、pulse.toml 改寫（toml_edit）、
│                      測試用的假 8444 伺服器（fake）
├── rotation.rs        8443 正常輪替：時點、待送的 CSR、驗證新憑證、確認後取代
├── decommission.rs    pulse decommission 的除役通知（8443 deregister）
├── backoff.rs         指數退避與抖動
├── log_files.rs       日誌按日分檔（.log／.jsonl）與保留清除（含系統日期異常時暫停清除）、日誌輸出（格式與 syslog 由 kp-obs 處理）
├── metrics.rs         本機指標（Prometheus）：計數、/metrics（loopback HTTP）與 textfile
├── diag.rs            pulse check-config 與 pulse diag（支援診斷包）
├── log_throttle.rs    日誌節流（同類訊息每個視窗最多記錄一則）
├── agent_id.rs        agent_id 產生、驗證與讀取
├── shutdown.rs        結束訊號監聽（Ctrl+C、Ctrl+Break、主控台關閉、系統關機）
├── system_info.rs     以 sysinfo 收集系統資訊、週期性取樣
└── test_log.rs        測試用：擷取日誌輸出（僅測試建置）
testdata/              測試用的憑證與私鑰（僅供測試、不可部署，見 testdata/README.md）：客戶端 CA / 中繼 CA / 客戶端憑證、
                       TLS 握手測試用的伺服器 CA 與伺服器憑證（server-ca.pem、server.pem、server.key）、
                       多個 DNS SAN 的客戶端憑證（multi-san.pem）、RSA 根 CA 的 8444 伺服器鏈（enroll-rsa-*）
config.toml            預設設定（編譯時嵌入；發佈包中改名為 pulse.example.toml）
build.rs               建置時決定 --version 顯示的 commit（BUILD_COMMIT 或 git）
packaging/             發佈包：build-release.sh（打包腳本）、pulse.service（systemd unit）、INSTALL.md（安裝指南）
```

## 日誌

- 同時輸出到終端（stdout，`log.stdout = false` 可關閉）與 `logs/pulse-YYYY-MM-DD.log`（相對於目前工作目錄）；`log.format = "json"` 時改為 `logs/pulse-YYYY-MM-DD.jsonl`
- **格式**（0.6，審計 O3；設計包 `pulse/log-ux.md` §6）：`text`（預設）為 `2026-10-14T14:22:16.123+08:00 [WARN] buffer.full PLS-W4202 本機緩衝已滿…`（RFC 3339、毫秒、帶時區位移；`log.timezone = "utc"` 時以 `Z` 結尾；0.5 以前行首是不帶時區的本地時間）；`json` 為一行一個 JSON（UTC），固定欄位 `v`、`ts`、`level`、`event`、`code`、`msg`、`agent_id`（啟用 mTLS 時才有值，未啟用時 agent_id 等同身分憑據，與狀態檔相同不寫出）、`instance`（多實例 `pulse-N.service`〔`StateDirectory=pulse/N`〕時為 N，單一實例為 `null`）、`product`、`host`、`run_id`（每次啟動隨機產生）、`seq`（本次行程內遞增）、`target`、`fields`；`legacy` 保留 0.5 以前的格式一個主要版本供過渡。`event`、`code` 取自訊息開頭的「`<event> <code> `」；尚未指定固定事件的行，`event` 為模組路徑（例如 `pulse.network`）
- **syslog 外送**：`[log.syslog]` 的 `target` 設定時另以 RFC 5424 送到 syslog（`udp://`、`tcp://`、`unix://`；`MSGID` 為錯誤代碼，沒有時為事件名稱；`payload = "json"` 時 `MSG` 為一行 JSON）。在背景執行緒送出，接收端無法連線時丟棄並計數，不影響服務與日誌檔
- **事件與錯誤代碼**：有固定事件的日誌行，訊息開頭為「`<event> <code> `」（沒有代碼時為 `-`），例如 `[WARN] buffer.full PLS-W4202 本機緩衝已滿…`、`[ERROR] auth.rejected PLS-E1105 連線中斷…`、`[INFO] conn.registered - 已向伺服器註冊…`。事件名稱與代碼固定、不翻譯（設計包 `pulse/log-ux.md` §4、`copywriting.md` §3.3），告警規則與搜尋請比對這兩欄（`json` 格式時為 `event`、`code` 欄位）。哪些代碼已對應、在程式的哪裡、何時觸發，見 `ERROR-CODES.md`；`log-ux.md` 的重試摘要排程、`log.lang` 等其餘改版尚未實作。本機緩衝的事件：`buffer.opened`（啟動時）、`buffer.unavailable`（`PLS-E4004`）、`buffer.write_failed`（`PLS-W4103`）、`buffer.full`（`PLS-W4202`）、`buffer.expired`（`PLS-W4203`）、`buffer.replay_done`（補送完成，附筆數與耗時）；連線恢復時另記 `conn.restored`（中斷時間、重試次數、待補送與丟棄筆數），中斷滿 30 分鐘記一次 `conn.outage`（`ERROR`，同時寫 stderr）
- **需要人工處理的錯誤另寫一行到 stderr**：伺服器憑證驗證錯誤（不受信任、名稱不符、過期、尚未生效、被撤銷、用途不符、被標記為 CA、其他驗證失敗；第一次發生就記 `ERROR`，原因改變時立即再記）、伺服器送來的致命錯誤與註冊前的錯誤（`certificate_rejected`、`auth_failed`、狀態表已滿等）、協定版本不符、連續被取代、客戶端憑證被伺服器以 TLS alert 拒絕（撤銷、過期等），以及本機客戶端憑證已過期。內容與日誌的 `ERROR` 行相同，節流也相同（同一原因每 10 分鐘最多一行，其餘只在日誌記 `WARN`）；一般連線錯誤（拒絕連線、逾時、連線被重設等）不寫。以 systemd 執行（有 `JOURNAL_STREAM` 環境變數）時行首加上 `<3>`，journal 以 err 等級記錄（`journalctl -u pulse -p err`），訊息中不會出現 `<3>`。stdout 與 stderr 都是終端機（互動執行）時不重複寫。`pulse.service` 設定 `StandardOutput=null`，所以 journal 中只會看到這些行與啟動失敗等 stderr 訊息
- **systemd 狀態文字**：Linux 上有 `NOTIFY_SOCKET` 時以 sd_notify 送出 `STATUS=`，`systemctl status` 顯示為 `Status:` 行：`連線中`（啟動後、第一次註冊或斷線前，含啟動時的隨機延遲）、`已註冊：<位址>:<連接埠>`、`重新連線中`（一般連線錯誤）、`無法連線：伺服器憑證<原因>（<處理提示>）`（例如 `無法連線：伺服器憑證不受信任（簽發者不在信任庫中，自簽憑證或私有 CA 請以 server.ca_cert 指定）`、`無法連線：伺服器憑證名稱不符（請把 server.domain 設為伺服器憑證 SAN 中的名稱）`；原因另有已過期、尚未生效、已被撤銷、用途不符、被標記為 CA、驗證失敗）、`無法註冊：<錯誤碼>（<處理提示>）`（錯誤碼如 `certificate_rejected`、`TLS alert CertificateRevoked`、`auth_failed`、`bad_request`、`unsupported_version`、`proto_mismatch`、`superseded`）。只送狀態文字、不送 `READY=1`，unit 維持 `Type=simple`，需設定 `NotifyAccess=main`（發佈包的 `pulse.service` 已設定）；沒有 `NOTIFY_SOCKET` 時（Windows、直接執行）不做任何事
- **按日分檔**：檔名日期為 **UTC**（與 Kairos 相同），UTC 午夜換新檔；`text` 每行開頭的時間是本機時間並帶時區位移，`json` 一律 UTC
- **保留**：保留今天與前 `log.retention_days` 天（預設 14），更早的檔案於啟動時與之後每小時刪除；`0` 表示永久保留。只會刪除檔名**完全符合** `pulse-YYYY-MM-DD.log`（或 `.jsonl`）的檔案，其他檔案（例如舊版的 `pulse.log`）一律不碰
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
- **本機緩衝**（見「本機緩衝」）：連上支援 `durable` 的 Kairos 時若有積壓，記 `INFO`「本機緩衝有 N 筆待補送的 report（最早的取樣時間 …），x 秒後依序補送…」，補完時記 `INFO`「已補送完本機緩衝中的積壓…」；緩衝已滿而丟掉最舊的資料、寫入或讀取緩衝失敗、超過 `max_age_hours` 而丟棄、被 Kairos 拒收而丟棄時記 `WARN`，各自每 5 分鐘最多一則（附累計筆數）
- **伺服器送來的文字一律清理後才寫入日誌**：`error` 的 `code`、`message`、無法解析的原始行，以及可能夾帶伺服器內容的 TLS／TCP／連線 I/O 錯誤文字（例如憑證名稱不符時列出的伺服器憑證 SAN）中，換行、ESC 等控制字元（Cc）、所有格式字元（Cf，例如零寬空格 U+200B、方向控制 U+202E、BOM U+FEFF）、U+2028 / U+2029，以及 Hangul filler、變體選擇字元、私用區等不可見碼位都會轉成跳脫字串（判定規則與 hostname 正規化、Kairos 的 hello 欄位驗證相同）（例如 `\n`、`\u{001b}`）；反斜線本身轉成兩個反斜線（`\\`），因此原文中的字面 `\n` 與被跳脫的換行可以區分。`code` 最多保留 64 個字元，TLS／I/O 錯誤文字最多 400 個字元，其他文字最多 200 個字元，超過時截斷並註明原始長度。避免惡意或故障的伺服器偽造日誌行、夾帶終端跳脫序列或以超長內容灌爆日誌
- **伺服器異常訊息節流**：伺服器送來無法解析的行、註冊後重複的 `hello_ack`、其他非致命 `error`、不合預期的 `ack`、因上一筆 report 尚未收到回應而略過取樣，這幾類訊息各自每 10 秒最多記錄一則；`storage_unavailable` 每 5 分鐘最多一則（伺服器的每日檔案大小上限滿時，每筆 report 都會收到，會持續到換日）。期間被略過的數量會在下一則一併註明，例如：

  ```
  [WARN] 已註冊後又收到 hello_ack（seq=1），忽略（自上次記錄以來另有 5737 則同類訊息被略過；同類訊息每 10 秒最多記錄一則）
  ```

  避免故障或惡意的伺服器在短時間內灌爆日誌檔。會結束連線的錯誤（fatal error、註冊前的 error）每次斷線只記一則（見「重連行為」），其頻率由重連退避限制；其中需要人工處理的錯誤另依錯誤碼節流 `ERROR`（每 10 分鐘最多一則，其餘記 `WARN`）。

## 本機指標（Prometheus，0.6.0）

預設關閉（審計 O3）。兩種提供方式，可同時使用：

```toml
[metrics]
listen = "127.0.0.1:9465"     # 本機 HTTP GET /metrics；只接受 loopback 位址（不需要認證，不對外開放）
textfile = "/var/lib/node_exporter/textfile_collector/pulse.prom"   # node_exporter textfile collector（建議）
textfile_interval = 15        # textfile 的更新間隔（秒，5～3600）
```

textfile 以「寫暫存檔再改名」的方式原子更新，Pulse 正常結束時刪除（避免留下過時的數值）；以 systemd 執行時需要以 drop-in 讓服務可以寫入該目錄（INSTALL.md「本機指標」）。

| 類別 | metric |
|---|---|
| 連線 | `pulse_connected`、`pulse_connection_state{state}`（`connecting`／`registered`／`reconnecting`／`rejected`）、`pulse_connection_consecutive_failures`、`pulse_registrations_total`、`pulse_disconnects_total{code}`（錯誤代碼，無法歸類為 `unknown`）；0.9.0 起另有 `pulse_server_candidates`（候選位址數）、`pulse_server_candidate_index`（目前連上的是第幾個，從 0 起）、`pulse_connect_failovers_total`（改試下一個位址的次數）、`pulse_drains_total`（因排空改連的次數）。不以位址當標籤 |
| report | `pulse_reports_sampled_total`、`pulse_reports_sent_total`（含補送）、`pulse_reports_acked_total`、`pulse_reports_rejected_total`、`pulse_reports_dropped_total{reason}`、`pulse_report_ack_duration_seconds`（直方圖：送出到收到回應） |
| 取樣 | `pulse_sample_duration_seconds`（直方圖）、`pulse_sample_timeouts_total`（`PLS-W8005`） |
| 本機緩衝（補送佇列） | `pulse_delivery_durable`、`pulse_buffer_enabled`、`pulse_buffer_available`、`pulse_buffer_pending_records`、`pulse_buffer_pending_bytes`、`pulse_buffer_max_bytes`、`pulse_buffer_max_records`、`pulse_buffer_usage_ratio`、`pulse_buffer_oldest_sample_timestamp_seconds`、`pulse_buffer_replaying`、`pulse_buffer_dropped_total{reason}`（`full`／`expired`／`rejected`）、`pulse_buffer_write_failures_total` |
| 其他 | `pulse_build_info{version,commit}`、`pulse_start_time_seconds`、`pulse_clock_offset_seconds`、`pulse_client_cert_expiry_timestamp_seconds`、`pulse_log_messages_total{level}`、`pulse_log_syslog_messages_total{result}` |

標籤只用有限集合的值，不含 agent_id 或主機名稱（由 Prometheus 的 `instance` 區分主機）。告警規則範例：`packaging/pulse-alerts.example.yml`。

## 設定檢查與診斷包（0.6.0）

```bash
# 檢查設定（不連線、不影響服務）：語法與範圍、伺服器、信任、用戶端憑證與私鑰（成對、到期、權限）、agent.id、
# 日誌、[metrics]、本機緩衝目錄。相對路徑以設定檔所在的目錄解析（與服務相同）
sudo pulse check-config -c /opt/pulse/pulse.toml        # 或 pulse --check-config
#   0 通過（有警告時仍為 0）、8 有警告（--strict）、3 設定錯誤、4 憑證或私鑰錯誤；--json 輸出 pulse.check-config/v1

# 支援診斷包（tar.gz，權限 0600）：pulse status 與 check-config 的輸出、設定（遮蔽後）、用戶端憑證的摘要、
# 註冊進度的摘要、最近 3 天的日誌（上限 50 MB）、本機指標、系統與服務資訊、相關檔案清單（不含內容）
cd /tmp && sudo pulse diag -c /opt/pulse/pulse.toml
sudo pulse diag -c /opt/pulse/pulse.toml --redact strict --since 7d   # 主機名稱與 IP 換成代號
```

私鑰（`client.key`）、token、`poll_secret` 永不收集；設定與日誌中的 token、密碼、PEM 私鑰區塊、`Bearer` 後的值一律遮蔽，產生前再檢查一次，仍有疑似秘密時不寫出（`PLS-E4301`）。Windows 也輸出 tar.gz（Windows 10 以上內建的 `tar.exe` 可解開）。`check-config --connect`、`--show-effective` 與 `diag --include-debug-log` 尚未實作。

## 目前限制

- 連上 Kairos 0.2.0 以前（協定 v1）時只回報 CPU 使用率與記憶體（v1 的三個欄位）。
- Windows 上的磁碟 I/O（`IOCTL_DISK_PERFORMANCE`）、行程清單與 SMBIOS 序號在虛擬服務帳號（`NT SERVICE\Pulse`）下的行為只經過交叉編譯，沒有在 Windows 上實測（METRICS.md 第 4.5 節的〔待實測〕）；讀不到磁碟 I/O 時不送 `system.disk.*` 並記 `PLS-I8009`，受保護行程的記憶體取不到時不列入記憶體排行。
- 主機資產的「上次送達的摘要」只存在記憶體中：Pulse 重新啟動後第一次連線一定再送一次（Kairos 以摘要判斷內容未變，不寫變更歷史）。IPv6 的暫時位址（privacy extensions）定期更換時會被視為資產改變而重送。
- 連上 Kairos 0.2.0 以前的版本，或 `buffer.enabled = false` 時，斷線期間、上一筆 report 尚未收到回應時，或伺服器無法保存（`storage_unavailable`）的 report 不會補送（at-most-once）。協商了 `durable` 時為 at-least-once，但本機緩衝滿了（丟最舊）、超過 `max_age_hours`、斷電前最近約 10 秒還沒 fsync 的取樣仍可能遺失；Kairos 的去重有範圍限制（SQLite 為原始資料保存的期間；`backend = "jsonl"` 為記憶體中的視窗，見 PROTOCOL.md 與 Kairos 的 STORAGE.md），極少數情況下可能重複寫入。`buffer.max_age_hours` 建議不超過 Kairos `storage.raw_retention_days` × 24。
- 本機緩衝不加鎖：同一個 `buffer.dir` 只能由一個 Pulse 使用。
- 多個伺服器位址（0.9.0）：8444 的註冊、輪詢與過期續簽不跟著候選位址走，只用 `enroll.address`（空字串時為 `server.address` 的主機）；叢集部署請把 `enroll.address` 設為負載平衡器或虛擬 IP 的名稱。沒有實作 DNS SRV（`server.srv`）。0.9.0 的 Kairos 還沒有共用資料庫的叢集模式，備用位址指到另一個各自有資料庫的 Kairos 時，換節點時在途的 report 可能兩邊各寫一次。
- Windows 服務（`pulse service`、MSI，0.8.0）只經過交叉編譯，未在 Windows 實機驗證。不寫 Windows 事件記錄（需要安裝程式註冊事件來源；事件識別碼已在 `codes.rs` 定案）。`pulse status` 在 Windows 上查詢的服務名稱固定為 `Pulse`，以服務包裝工具註冊成其他名稱時改以狀態檔的 PID 判斷。MSI 沒有安裝字串的參數、移除時不會自動 `pulse decommission`。
- `pulse status` 尚不支援 `--all`（列出所有實例）與 `--json-lines`；「連線中斷 30 分鐘」的門檻固定。
- 系統資訊收集任務異常時直接結束程式（不在程式內自動重啟），需搭配服務管理員。
- 取樣卡住（例如 WMI 無回應）時只在本機記 `WARN` 並略過 report，heartbeat 照常送出；Kairos 目前不會因 `last_report` 停滯而告警（見「回報資料格式」）。
- 兩台主機共用同一份 `agent.id` 時，Pulse 只能拉長重連間隔（連續被取代時至少 30 秒）並以 `ERROR` 提示，無法自動解決，雙方仍會持續互踢。
- `hostname` 只供顯示：送出前會把控制字元、格式字元（零寬空格、方向控制等）、U+2028 / U+2029 與不可見碼位（Hangul filler、變體選擇字元、私用區等）替換成 `?`，並截到 255 bytes 以內，伺服器上顯示的名稱可能與實際主機名稱略有不同（識別以 `agent_id` 為準）。
- 註冊（`pulse enroll`）：沒有實作 `enroll.env` 的自動註冊、MSI 的串接（Linux 的安裝腳本 `pulse-install.sh` 已串接，0.8.0）、`--progress` 的原地重繪與 `PULSE_LANG`；Windows 的私鑰 ACL、服務重新啟動（`sc.exe`）只經過交叉編譯，沒有在 Windows 上實測。`pulse enroll` 不初始化日誌檔，`admin.dangerous_action` 等事件不會寫進 `logs/`。
- 8444 本機時鐘偏差的例外（忽略伺服器憑證有效期間只取 `server_time`）只用於 `pulse enroll` 的錯誤訊息；服務端憑證到期判定仍以 Kairos 的 `CertificateExpired` 為準，沒有以 8444 `server-info` 校正（設計包 §8.1 (d)）。
- Windows 上的檔案權限檢查只看 ACL 中授權給一般使用者群組的「允許」項目，不計算「拒絕」項目與實際有效權限，也不展開自訂群組（見「敏感檔案的權限檢查」）。
- 集中設定（0.9.0）：即時的取樣設定改變時重新開始取樣（下一筆在一個新的間隔之後）；`retry.*`、`buffer.replay_interval_ms` 與 hello 的 `report_interval_ms` 在下一次連線生效。執行中修改 `pulse.toml` 仍需重新啟動 Pulse。
- 遠端升級：只支援 Linux（Windows 的 MSI／zip 尚未提供）。外部 URL 下載自 0.10.0 起提供（`[upgrade] url_allowlist`，預設空＝只從 Kairos 下載；`pulse/packaging/INSTALL.md` 第 10.1 節）。deb／rpm 的回滾檔只在本機保存過它所屬發佈的驗證紀錄時使用（從 0.9.0 手動安裝後的第一次遠端升級沒有，回滾時以 `pulse.prev` 回復、套件資料庫仍顯示新版，之後以套件管理員重新安裝即可一致）。

## 授權

尚未指定。
