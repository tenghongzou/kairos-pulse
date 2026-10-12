# Pulse 安裝指南（從發佈包安裝）

本文件給地端管理者使用：從預編譯的發佈包把 Pulse 安裝成**系統層級 systemd 服務**，連到中央伺服器 Kairos。功能、設定欄位與協定細節請見同目錄的 `README.md` 與 `PROTOCOL.md`。

> 不要用 `systemd-run --user` 或使用者層級的 unit 跑正式服務：沒有開啟 linger 時，最後一個登入 session 結束，user manager 會連同 Pulse 一起被停掉。

## 0. 發佈包內容

發佈包 `pulse-<版本>-<commit>-linux-x86_64.tar.gz` 內含一層 `pulse-<版本>-<commit>-linux-x86_64/` 目錄：

| 檔案 | 權限 | 說明 |
|---|---|---|
| `pulse` | 755 | 執行檔（x86_64、musl 靜態連結，不依賴系統函式庫） |
| `pulse.example.toml` | 644 | **內建預設值的參考**（見第 6 節），不需要複製或修改 |
| `pulse.service` | 644 | systemd unit 範本（單一實例） |
| `INSTALL.md` | 644 | 本文件 |
| `README.md`、`PROTOCOL.md` | 644 | 說明文件與協定規格 |
| `CHANGELOG.md` | 644 | 變更紀錄；升級前請先看「破壞性變更與升級步驟」 |
| `THIRD-PARTY-LICENSES` | 644 | 執行檔內含的第三方套件清單與授權全文（建置時由 `cargo about` 依 `Cargo.lock` 產生） |
| `pulse.cdx.json` | 644 | SBOM（CycloneDX 1.5 JSON，建置時由 `cargo cyclonedx` 產生）；發佈處另附一份同內容的 `pulse-<版本>-<commit>-linux-x86_64.cdx.json`，可直接匯入 Dependency-Track 等工具 |

檔名中的 `<版本>` 是 SemVer 版本號（與 `--version` 相同，版本規則見 `CHANGELOG.md`），`<commit>` 是建置時的 git commit；檔名帶 `-dirty` 表示從有未提交變更的工作目錄建置，**不應**用於正式環境。

以下範例的路徑與名稱：

| 項目 | 值 |
|---|---|
| 安裝目錄 | `/opt/pulse`（擁有者 `root:pulse`，750） |
| 服務使用者 | `pulse`（系統帳號，不能登入） |
| 可寫目錄 | `/opt/pulse/logs`（擁有者 `pulse`） |
| Kairos | `monitor.example.com:8080`（換成 Kairos 管理者提供的位址與憑證名稱） |

**從第 2 節起，請在同一個 root shell 中依序執行**：

```bash
sudo -i          # 之後的指令都在這個 shell 中執行；本文件的 ~ 指 root 的家目錄（/root）
```

不能只在每個指令前加 `sudo`：

- `/opt/pulse` 的權限是 750（第 4 節），一般帳號無法 `cd` 進去，而 `sudo cd` 不存在。
- 第 5 節的 `agent.id`、`client.key` 與 CSR 都寫到**目前目錄**，必須先 `cd /opt/pulse`；在別的目錄執行會把 `agent.id` 寫到錯的地方，服務啟動時讀不到（`/opt/pulse` 在服務中唯讀，無法自行建立）。
- 第 5 節設定的 `AGENT_ID`、`umask` 只在同一個 shell 中有效。
- `/opt/pulse/pulse --version`、讀取 `logs/` 等確認步驟也需要 root（一般帳號會 `Permission denied`）。

中途離開了 root shell 時，重新 `sudo -i`，並從該節開頭（含 `cd`）重新執行。

## 1. 需求

- Linux x86_64、systemd（建議 247 以上）
- 能以 TCP 連到 Kairos 的監聽埠
- 產生私鑰與 CSR 用的 OpenSSL（1.1.1 以上即可；沒有的話見第 5 節的替代做法）
- 驗證發佈包簽章用的 OpenSSH 8.1 以上（`ssh -V`；可在管理主機上驗證，見第 2 節）
- 從 Kairos 管理者取得：Kairos 的位址與 `server.domain`（伺服器憑證 SAN 中的名稱）、伺服器 CA 憑證 `ca.pem`；啟用 mTLS 時還要由對方簽發客戶端憑證

## 2. 下載與驗證

從發佈處取得下列檔案，放在同一個目錄：

| 檔案 | 說明 |
|---|---|
| `pulse-<版本>-<commit>-linux-x86_64.tar.gz` | 發佈包 |
| `SHA256SUMS` | Kairos 與 Pulse 共用的雜湊清單（同一次發佈的所有檔案） |
| `SHA256SUMS.sig` | `SHA256SUMS` 的簽章（`ssh-keygen -Y sign`） |
| `allowed_signers` | 發佈公鑰。**必須從另一個管道取得**，見下方說明 |

```bash
mkdir -p ~/pulse-dist && cd ~/pulse-dist
# 1. 驗證 SHA256SUMS 的簽章：必須顯示
#    Good "kairos-pulse-release" signature for release@kairos-pulse with ED25519 key SHA256:…
ssh-keygen -Y verify -f allowed_signers -I release@kairos-pulse -n kairos-pulse-release \
  -s SHA256SUMS.sig < SHA256SUMS
# 2. 驗證發佈包的雜湊：必須顯示「pulse-…tar.gz: OK」
sha256sum -c --ignore-missing SHA256SUMS
```

任一步失敗（`Could not verify signature`、`FAILED`）都不要解壓，請向供應商確認。`--ignore-missing` 是因為 `SHA256SUMS` 也列了 Kairos 的發佈包與 SBOM；只要你下載的發佈包顯示 `OK` 即可。

**公鑰必須從另一個管道取得**：簽章只能證明 `SHA256SUMS` 由持有發佈私鑰的人簽署。若 `allowed_signers` 和發佈包從同一個地方下載，能替換發佈包的人也能一起替換公鑰，驗證就沒有意義。請從供應商另外提供的管道取得 `allowed_signers`（例如合約或交付文件、客戶入口網站的 HTTPS 頁面），第一次使用時再以電話等已知可信的方式核對指紋；之後保存在管理主機上重複使用，供應商更換金鑰時會另行通知。

```bash
awk '{print $3, $4}' allowed_signers | ssh-keygen -lf -   # 印出 256 SHA256:… (ED25519)，與供應商提供的指紋比對
```

`ssh-keygen -Y verify` 需要 OpenSSH 8.1 以上（`ssh -V`）。RHEL／Rocky 8（OpenSSH 8.0）、Debian 10 等較舊的系統沒有這個功能，請在管理主機（Windows 11 內建的 OpenSSH 或 Git for Windows 的 `ssh-keygen`、macOS、Ubuntu 20.04 以上、Debian 11 以上、RHEL 9）完成驗證後，再把發佈包複製到目標主機。

## 3. 解壓並檢查執行檔

執行檔在第 4 節才安裝到 `/opt/pulse`；本節只解壓並確認版本。

```bash
cd ~/pulse-dist
mkdir -p pkg
tar xzf pulse-<版本>-<commit>-linux-x86_64.tar.gz -C pkg --strip-components=1
file pkg/pulse               # 應為 ELF 64-bit x86-64 static-pie linked
pkg/pulse --version          # 輸出 pulse <版本> (<commit>)，commit 應與檔名一致
```

tar 內有一層目錄，所以要加 `--strip-components=1`。

## 4. 建立使用者與目錄

```bash
useradd --system --user-group --home-dir /opt/pulse --no-create-home \
  --shell /usr/sbin/nologin pulse

install -d -o root  -g pulse -m 750 /opt/pulse
install -d -o pulse -g pulse -m 750 /opt/pulse/logs

cd ~/pulse-dist/pkg
install -o root -g root -m 755 pulse /opt/pulse/pulse
install -o root -g root -m 644 pulse.example.toml INSTALL.md README.md PROTOCOL.md \
  CHANGELOG.md THIRD-PARTY-LICENSES /opt/pulse/
```

`/opt/pulse` 本身屬於 root，pulse 使用者只能讀取執行檔、設定與憑證；unit 以 `ProtectSystem=strict` 讓服務只能寫 `logs/`。**`logs/` 必須在啟動前存在**，否則 unit 以 `226/NAMESPACE` 啟動失敗。

## 5. 產生 agent_id 並申請客戶端憑證

### 5.1 產生 agent_id

每個 Pulse 實例有一個固定的 `agent_id`（UUID v4），存在工作目錄的 `agent.id`。服務執行時 `/opt/pulse` 是唯讀的，所以**安裝時先產生**。

> 5.1 與 5.2 必須在**同一個 root shell**（`sudo -i`，見開頭說明）中連續執行：`agent.id`、`client.key` 與 CSR 都寫到目前目錄 `/opt/pulse`，`AGENT_ID` 變數也要延續到 5.2。

```bash
cd /opt/pulse                         # 必須在實際的工作目錄執行，才會寫到同一份 agent.id
./pulse --print-agent-id              # stdout 印出 agent_id（小寫），「已產生新的 agent_id」提示在 stderr
chown root:pulse agent.id             # 服務帳號只能讀、不能改
chmod 640 agent.id
AGENT_ID=$(./pulse --print-agent-id)  # 之後的步驟會用到
echo "$AGENT_ID"
```

- `agent.id` 已存在時只會讀取，不會改變。
- **不要**把 `agent.id` 複製到其他主機（例如複製 VM 映像檔後忘了刪）：兩台會互相把對方踢下線（`superseded`）。複製出來的主機請刪除 `agent.id` 後重做本節。
- 未啟用 mTLS 時，`agent.id` 的內容就是本機在伺服器上的身分：擁有者 root、群組 pulse、權限 640，只有 root 能修改，服務帳號 pulse 只能讀取。Pulse 讀到既有的 `agent.id` 後不會寫回，因此唯讀即可正常運作；權限過寬（例如其他使用者可讀）時啟動日誌會記 `WARN`。

### 5.2 產生私鑰與 CSR，交給 Kairos 管理者簽發

私鑰在 Pulse 主機上產生，**不離開這台主機**；只把 CSR 與 agent_id 交給 Kairos 管理者：

```bash
cd /opt/pulse
AGENT_ID=$(./pulse --print-agent-id)  # 與 5.1 相同的值；重新取得，避免沿用到其他實例的值
umask 077
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout client.key -out "$AGENT_ID.csr" -subj "/CN=$AGENT_ID" \
  -addext "subjectAltName=DNS:$AGENT_ID" \
  -addext "extendedKeyUsage=clientAuth"
chown root:pulse client.key           # 服務帳號只能讀、不能改
chmod 640 client.key

# 取出 CSR：/opt/pulse 內的 CSR 是 root 600，複製一份可讀的到 /tmp 再傳給管理者
install -m 644 "$AGENT_ID.csr" /tmp/
ls -l "/tmp/$AGENT_ID.csr"
```

- 也可以用 RSA：把 `-newkey ec -pkeyopt ec_paramgen_curve:P-256` 換成 `-newkey rsa:2048`。私鑰不可加密（`-nodes`）。
- `-addext` 需要 OpenSSL 1.1.1 以上；更舊的版本請改用下面的替代做法。
- CSR 不含私鑰，可以公開傳遞；從 `/tmp/<agent_id>.csr` 以一般帳號 `scp` 取出，或在 root shell 中 `cat "$AGENT_ID.csr"` 複製貼上。**不要**把 `client.key` 複製出 `/opt/pulse`。
- 把 `$AGENT_ID.csr` 與 agent_id 交給 Kairos 管理者。對方以代理 CA 的 `openssl ca` 資料庫簽發（見 Kairos 的 `INSTALL.md`「簽發代理的客戶端憑證」），這樣之後才能**撤銷**；簽發時 SAN 會強制設為 `DNS:<agent_id>`。
- 取回兩個檔案：客戶端憑證（存成 `/opt/pulse/client.pem`）與伺服器 CA（存成 `/opt/pulse/ca.pem`）。

替代做法（Pulse 主機上沒有可用的 OpenSSL）：由 Kairos 管理者代為產生私鑰並簽發，再以安全的管道（例如 `scp`，不要用 email）把 `client.key` 傳到這台主機，傳完後刪除管理者端的副本。

### 5.3 未啟用 mTLS 的 Kairos

Kairos 未設定 `auth.client_ca` 時不需要客戶端憑證，略過 5.2，只需要 `ca.pem`。這只建議用於測試環境：任何人知道 agent_id 就能冒用本機。

## 6. 建立 pulse.toml

Pulse 的設定分三層，後者覆蓋前者：

1. **內建預設值**：編譯時嵌入執行檔。發佈包中的 `pulse.example.toml` 就是這份內容，只供參考，程式不會讀取這個檔名；**不要**修改或整份複製它。
2. **`/opt/pulse/pulse.toml`**：只寫要覆蓋的欄位。
3. 環境變數 `PULSE__<SECTION>__<KEY>`（可用 `systemctl edit pulse` 加 `Environment=`）。

```bash
install -o root -g pulse -m 640 /dev/null /opt/pulse/pulse.toml
```

編輯 `/opt/pulse/pulse.toml`：

```toml
[server]
address = "monitor.example.com"   # Kairos 的位址（主機名稱或 IP）
port = 8080
domain = "monitor.example.com"    # 必須是 Kairos 伺服器憑證 SAN 中的名稱
ca_cert = "ca.pem"                # 伺服器 CA；設定後預設只信任它，不信任作業系統信任庫
client_cert = "client.pem"        # mTLS（5.3 的情況下刪掉這兩行）
client_key = "client.key"
```

相對路徑以工作目錄 `/opt/pulse` 解析。設定了 `ca_cert` 時 Pulse 預設只信任它（`server.use_system_roots` 不必設定；舊版範例中的 `use_system_roots = false` 可以保留，效果相同）。心跳、回報間隔等其他欄位維持預設即可（見 `README.md`「設定」）。

### 6.1 檔案權限

在 root shell 中，先 `cd` 到取回的 `ca.pem`、`client.pem` 所在的目錄（例如以一般帳號 `scp` 到 `/home/<帳號>/pulse-files/`）：

```bash
cd /home/<帳號>/pulse-files                                # 換成實際的目錄
install -o root -g pulse -m 644 ca.pem     /opt/pulse/ca.pem
install -o root -g pulse -m 644 client.pem /opt/pulse/client.pem
```

| 路徑 | 擁有者 | 權限 |
|---|---|---|
| `/opt/pulse` | `root:pulse` | 750 |
| `pulse`（執行檔） | `root:root` | 755 |
| `pulse.toml` | `root:pulse` | 640 |
| `ca.pem`、`client.pem` | `root:pulse` | 644 |
| `client.key`、`agent.id` | `root:pulse` | 640 |
| `logs/` | `pulse:pulse` | 750 |

`$AGENT_ID.csr`（`/opt/pulse` 內與 `/tmp` 的副本）交出去後可以刪除。

`client.key` 與 `agent.id` 由 root 擁有、群組 pulse 只能讀（640）：即使服務帳號被入侵，也無法竄改本機身分或替換私鑰（unit 的 `ProtectSystem=strict` 另外讓 `/opt/pulse` 對服務唯讀）。Pulse 啟動時檢查這兩個檔案的權限：其他使用者有任何權限、群組可寫入，或擁有者不是 root 而群組可讀時記 `WARN`（不阻止啟動）。舊版 INSTALL 的 `pulse:pulse 600` 仍可正常運作、不會警告，建議改成新的配置（見第 10 節）。

## 7. 安裝 systemd unit

```bash
install -o root -g root -m 644 ~/pulse-dist/pkg/pulse.service /etc/systemd/system/pulse.service
systemctl daemon-reload
systemd-analyze verify /etc/systemd/system/pulse.service    # 沒有輸出表示格式正確
```

unit 重點（內容見 `pulse.service`）：

| 設定 | 值 | 理由 |
|---|---|---|
| `User`／`Group` | `pulse` | 不以 root 執行 |
| `WorkingDirectory` | `/opt/pulse` | `pulse.toml`、`agent.id`、憑證與 `logs/` 都以工作目錄解析 |
| `StandardOutput` | `null` | 日誌只保留 `logs/` 一份（理由見下） |
| `KillSignal`／`TimeoutStopSec` | `SIGINT`／`10` | Pulse 送出 `close_notify` 後約 1 秒內結束 |
| `Restart` | `always` | Pulse 本身會無限重連；取樣任務異常等內部錯誤（結束代碼 1）時由 systemd 重啟 |
| `RestartPreventExitStatus` | `2 3 4` | 重試也不會成功的錯誤不重啟：2 命令列參數錯誤、3 設定錯誤（`pulse.toml` 語法、型別、範圍錯誤等）、4 憑證錯誤（`ca_cert`、客戶端憑證或私鑰無法讀取、格式錯誤、不成對，SAN 與 agent_id 不符，`agent.id` 無法讀取或內容不合法）。`systemctl status pulse` 顯示 `status=2`／`3`／`4`，原因在 `journalctl -u pulse`，修正後 `systemctl restart pulse` |
| `NotifyAccess` | `main` | 只用來以 sd_notify 更新 `systemctl status` 的 `Status:` 文字（見 8.1）；不送 `READY=1`，所以 `Type` 仍為 `simple` |
| `ProtectSystem=strict` + `ReadWritePaths=/opt/pulse/logs` | | 其餘檔案系統唯讀；因此 `agent.id` 必須在安裝時產生（第 5.1 節） |

### 7.1 為什麼選擇 `StandardOutput=null`

Pulse 把日誌同時寫到 stdout 與 `logs/pulse-YYYY-MM-DD.log`。systemd 預設把 stdout 收進 journal，結果同一份日誌存兩份。本範本選擇只保留 `logs/`：

- `logs/` 依 UTC 日期分檔，並由 Pulse 依 `log.retention_days`（預設 14 天）自動清除，空間可預期；journal 的保留則取決於整台主機的 journald 設定，大量 agent 主機上不易個別控制。
- 檔案格式與 Windows 等其他平台相同，蒐集與排查的方式一致，也與 Kairos 端的 `logs/` 對得起來。
- stderr 仍進 journal（`StandardError=journal`）：設定錯誤、日誌檔無法建立、panic 這類**日誌檔還沒開好就發生**的錯誤都寫在 stderr；憑證錯誤等日誌初始化之後才發生、使 Pulse 結束的錯誤，也會另寫一行 `[ERROR] …` 到 stderr，所以啟動失敗時 `journalctl -u pulse` 仍看得到原因。

想改成只用 journal：以 `systemctl edit pulse` 加上 `[Service]`、`StandardOutput=journal`，並視需要把 `log.retention_days` 調小（`logs/` 仍會寫入，Pulse 沒有關閉檔案日誌的設定）。

## 8. 啟動與確認

```bash
systemctl enable --now pulse
systemctl status pulse --no-pager
```

確認清單（在 root shell 中執行；`/opt/pulse` 為 750，一般帳號執行會 `Permission denied`）：

```bash
/opt/pulse/pulse --version                      # pulse <版本> (<commit>)
journalctl -u pulse -n 20 --no-pager            # 只有 systemd 啟停紀錄與 stderr；沒有錯誤即正常
tail -n 30 /opt/pulse/logs/pulse-$(date -u +%F).log
```

日誌中應依序出現：

```
[INFO] Pulse 地端程式啟動（pulse <版本> (<commit>)）...
[INFO] 工作目錄：/opt/pulse（設定中的相對路徑都以此為準）；設定檔：已讀取 /opt/pulse/pulse.toml
[INFO] 已載入客戶端憑證：client.pem（憑證鏈 1 張；私鑰：client.key）
[INFO] 已向伺服器註冊（agent_id=…，伺服器離線判定 75000 ms；TLS 加密連線 monitor.example.com:8080，domain=monitor.example.com，TLSv1_3，已驗證伺服器憑證）
```

- 啟動時 Pulse 會先隨機等待 0～5 秒（「x 秒後開始連線」）才連線，避免大量主機同時連入。
- 判斷連線成功以「已向伺服器註冊」這一行為準。伺服器離線判定為心跳間隔（預設 15 秒）的 5 倍：主機斷線約 75 秒後才會在 Kairos 顯示為離線。TLS 握手完成（「已建立 TLS 加密連線」）只在 `debug` 等級記錄，因為 TLS 1.3 下伺服器在握手後才驗證客戶端憑證，握手完成不代表會被接受。
- 這一行若是 `WARN` 且含「⚠️ 未驗證伺服器憑證」，表示設定了 `accept_invalid_certs = true`，正式環境不可如此。
- 啟動時的系統資訊在 `info` 等級只列實體磁碟與網路介面（略過虛擬檔案系統、容量 0 的掛載點、與已列出磁碟相同的重複掛載點（例如本 unit 的 `ReadWritePaths=`、`PrivateTmp=` 在服務內建立的 `/opt/pulse/logs`、`/tmp`、`/var/tmp` bind mount）、`lo` 與 `veth*`、`docker*`、`br-*` 等虛擬介面，各以一行統計），CPU 只有一行摘要；完整清單需 `log.level = "debug"`。

最後請 Kairos 管理者確認 Kairos 主機上的 `/opt/kairos/data/agents.json` 有這個 agent_id，且 `"online": true`。

常見的失敗原因：

| 日誌 | 處理 |
|---|---|
| `設定檔：/opt/pulse/pulse.toml 不存在，只使用內建預設值…` | 檔名或位置錯誤。檔案存在但 pulse 使用者沒有讀取權限時，Pulse 會以「無法載入設定」結束，原因在 `journalctl -u pulse` |
| `TCP 連線失敗` 持續出現 | 位址、連接埠或防火牆；Kairos 預設只聽 127.0.0.1 |
| `伺服器憑證的名稱與 server.domain「…」不符` | `server.domain` 要填伺服器憑證 SAN 中的名稱。伺服器憑證的問題服務仍為 active，`Status:` 為 `無法連線：伺服器憑證…`（見 8.1） |
| `伺服器憑證不受信任（簽發者不在信任庫中）` | `ca_cert` 不是簽發 Kairos 伺服器憑證的 CA |
| `伺服器憑證已過期`／`尚未生效`／`已被撤銷`／`被標記為 CA`、`伺服器憑證的用途（EKU）不含 serverAuth` | 請伺服器管理者更換憑證；過期或尚未生效時也檢查本機時鐘 |
| 啟動即結束：`SAN 不含 DNS:<agent_id>` | 憑證不是用這台的 agent_id 簽發（例如 `agent.id` 重新產生過），重做 5.2 |
| `伺服器表示此客戶端憑證已被撤銷`、`certificate_rejected` | 憑證已被撤銷，請管理者重新簽發。Pulse 會持續重連，服務仍為 active（見 8.1） |
| `無法初始化日誌`（journal） | `logs/` 不存在或不屬於 pulse |

其他訊息見 `README.md`「啟用 mTLS」與「重連行為」。

### 8.1 需要人工處理的錯誤：服務仍為 active (running)

本機憑證被撤銷、身分驗證失敗、伺服器憑證不受信任或名稱不符這類**需要人工處理**的錯誤，Pulse 不會結束，而是持續退避重連，所以 systemd 看到的服務仍是 `active (running)`、`NRestarts` 不會增加。只看 `systemctl is-active` 的監控會漏掉，請用下列方式確認：

1. **`systemctl status pulse` 的 `Status:` 行**：Pulse 以 sd_notify 更新狀態文字（監控腳本可用 `systemctl show -p StatusText --value pulse` 取得同一段文字）：

   | `Status:` | 意義 |
   |---|---|
   | `連線中` | 啟動後尚未完成第一次連線（含啟動時 0～5 秒的隨機延遲） |
   | `已註冊：monitor.example.com:8080` | 已向伺服器註冊（IPv6 位址加方括號） |
   | `重新連線中` | 一般連線錯誤（拒絕連線、逾時、連線被重設、伺服器關閉連線等），正在退避重連 |
   | `無法連線：伺服器憑證不受信任（簽發者不在信任庫中，自簽憑證或私有 CA 請以 server.ca_cert 指定）` | `server.ca_cert` 不是簽發 Kairos 伺服器憑證的 CA |
   | `無法連線：伺服器憑證名稱不符（請把 server.domain 設為伺服器憑證 SAN 中的名稱）` | `server.domain` 與伺服器憑證 SAN 不符 |
   | `無法連線：伺服器憑證已過期（…）`、`…尚未生效（…）` | 伺服器憑證過期或尚未生效，或本機時鐘不正確 |
   | `無法連線：伺服器憑證已被撤銷（…）`、`…用途不符（…）`、`…被標記為 CA（…）`、`…驗證失敗（詳見 logs/）` | 伺服器憑證本身的問題，請伺服器管理者更換 |
   | `無法註冊：certificate_rejected（…）` | 伺服器拒絕本機憑證（撤銷、過期、不受信任）；括號內為 `伺服器的 CRL 已過期…` 時是伺服器端問題 |
   | `無法註冊：TLS alert CertificateRevoked（…）` | 憑證被撤銷後的重連在 TLS 握手時就被拒；其他 alert 如 `CertificateExpired`、`CertificateRequired`、`UnknownCA` 等 |
   | `無法註冊：auth_failed（…）` | 憑證 SAN 與 agent_id 不符 |
   | `無法註冊：bad_request（伺服器狀態表已滿…）` | Kairos 的 `limits.max_agents` 已滿 |
   | `無法註冊：unsupported_version（…）`、`無法註冊：proto_mismatch（…）` | 協定版本不符，請更新 Pulse 或 Kairos |
   | `無法註冊：superseded（…）` | 連續被同一 agent_id 的其他連線取代，多台主機可能共用同一份 `agent.id` |

   括號內是簡短的處理提示，完整訊息在日誌。狀態文字會隨每次重連更新：例如憑證被撤銷時，`無法註冊：certificate_rejected（…）` 只維持到下一次重連（約 2～7 秒），之後都是 `無法註冊：TLS alert CertificateRevoked（…）`；Kairos 停機時，即使本機憑證已被撤銷，也只會顯示 `重新連線中`（連不到伺服器就無法得知是否被撤銷）。監控腳本請以 `無法註冊：`、`無法連線：` 開頭判斷，不要比對完整字串。`Status:` 行需要 unit 中的 `NotifyAccess=main`（只用來更新狀態文字，`Type` 仍為 `simple`）；**舊版 unit 沒有這一行時不會出現 `Status:` 行**，升級時請一併更新 unit（第 10 節）。

2. **`journalctl -u pulse -p err`**：上述錯誤（伺服器憑證驗證失敗、伺服器送來的致命錯誤與註冊前的錯誤、協定版本不符、連續被取代、客戶端憑證被伺服器以 TLS alert 拒絕），以及本機客戶端憑證已過期，會在寫 `ERROR` 日誌的同時寫一行到 stderr，內容與日誌的 `ERROR` 行相同，在 journal 中以 err 等級記錄，可用 `-p err` 篩選。第一次發生就記 `ERROR`，同一原因每 10 分鐘最多一行（其餘次數在 `logs/` 記 `WARN`），原因改變時立即再記。一般連線錯誤（`重新連線中`：拒絕連線、逾時、連線被重設等）不寫 stderr，只在 `logs/`。

   ```
   pulse[1234]: [ERROR] 連線中斷（連續第 1 次失敗）：伺服器回報致命錯誤 certificate_rejected：…；2.0 秒後重連
   pulse[1234]: [ERROR] 連線中斷（連續第 1 次失敗）：TLS 握手失敗：invalid peer certificate: UnknownIssuer（伺服器憑證不受信任（簽發者不在信任庫中）：自簽憑證或私有 CA 請以 server.ca_cert 指定）；2.0 秒後重連
   ```

3. **`logs/pulse-YYYY-MM-DD.log`**：每次斷線都有一行完整紀錄。
4. **Kairos 端**：`agents.json` 中此 agent 是否為 `"online": true`。

## 9. 多實例（同一台主機跑多個 Pulse）

一般一台主機只跑一個 Pulse。測試等情況需要多個實例時，每個實例必須有**自己的工作目錄、`agent.id`、客戶端憑證與 `logs/`**（共用 `agent.id` 會互相踢下線）。

前提：第一個實例（`/opt/pulse`、`pulse.service`）已照第 4～8 節裝好。之後的實例共用使用者 `pulse` 與執行檔 `/opt/pulse/pulse`，其餘都放在 `/opt/pulse-N`（N 為 2、3…）。

> 第 5、6 節的指令都寫死 `/opt/pulse`。替其他實例做時，**所有 `/opt/pulse` 路徑都要換成 `/opt/pulse-N`**（只有執行檔 `/opt/pulse/pulse` 例外），漏改會覆寫第一個實例的 `agent.id`、私鑰或設定。下面列出完整指令，請在**同一個 root shell**（`sudo -i`）中依序執行。

```bash
N=2                                   # 每個實例換一個編號；以下所有步驟都依賴這個變數

# 1. 目錄（第 4 節）
install -d -o root  -g pulse -m 750 /opt/pulse-$N
install -d -o pulse -g pulse -m 750 /opt/pulse-$N/logs

# 2. unit：只改 WorkingDirectory 與 ReadWritePaths 兩行；
#    ExecStart 仍指向共用的執行檔，註解與 Documentation= 不變
sed -e "/^\(WorkingDirectory\|ReadWritePaths\)=/s#/opt/pulse#/opt/pulse-$N#" \
    /etc/systemd/system/pulse.service > /etc/systemd/system/pulse-$N.service
chmod 644 /etc/systemd/system/pulse-$N.service
grep -E '^(WorkingDirectory|ReadWritePaths|ExecStart)=' /etc/systemd/system/pulse-$N.service
#   WorkingDirectory=/opt/pulse-2
#   ExecStart=/opt/pulse/pulse
#   ReadWritePaths=/opt/pulse-2/logs

# 3. agent_id（第 5.1 節，在 /opt/pulse-N 執行）
cd /opt/pulse-$N
/opt/pulse/pulse --print-agent-id
chown root:pulse agent.id
chmod 640 agent.id
AGENT_ID=$(/opt/pulse/pulse --print-agent-id)   # 這個實例自己的 agent_id
echo "$AGENT_ID"

# 4. 私鑰與 CSR（第 5.2 節，仍在 /opt/pulse-N）
umask 077
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout client.key -out "$AGENT_ID.csr" -subj "/CN=$AGENT_ID" \
  -addext "subjectAltName=DNS:$AGENT_ID" \
  -addext "extendedKeyUsage=clientAuth"
chown root:pulse client.key
chmod 640 client.key
install -m 644 "$AGENT_ID.csr" /tmp/  # 把 /tmp/<agent_id>.csr 交給 Kairos 管理者簽發
```

取回這個實例的 `client.pem` 後（同一個 root shell，`N` 仍要是同一個值）：

```bash
# 5. 憑證與設定（第 6、6.1 節）
cd /home/<帳號>/pulse-files-$N                                # 這個實例的 client.pem 所在目錄；每個實例用不同目錄，避免同名的 client.pem 互相覆蓋
install -o root -g pulse -m 644 client.pem          /opt/pulse-$N/client.pem
install -o root -g pulse -m 644 /opt/pulse/ca.pem   /opt/pulse-$N/ca.pem
# pulse.toml 中的路徑都是相對於工作目錄，可直接沿用第一個實例的設定
install -o root -g pulse -m 640 /opt/pulse/pulse.toml /opt/pulse-$N/pulse.toml

# 6. 啟動與確認（第 7、8 節）
systemctl daemon-reload
systemd-analyze verify /etc/systemd/system/pulse-$N.service   # 沒有輸出表示格式正確
systemctl enable --now pulse-$N
tail -n 30 /opt/pulse-$N/logs/pulse-$(date -u +%F).log       # 「工作目錄：/opt/pulse-N」與「已向伺服器註冊」
```

- 每個實例的 agent_id 不同，客戶端憑證要分別簽發；不要把第一個實例的 `client.pem`、`client.key` 複製過去（SAN 不符，啟動即結束）。
- 以 `systemctl edit pulse` 建立的 drop-in 只套用到 `pulse.service`，`pulse-N` 需要時要各自 `systemctl edit pulse-N`。
- 升級後 `pulse.service` 有變動時，重做步驟 2 重新產生 `pulse-N.service`，再 `systemctl daemon-reload`。

多個實例都從同一個 IP 連入 Kairos，Kairos 端可能需要調高 `server.max_connections_per_ip`（見 Kairos 的 `README.md`）。

## 10. 升級

**升級順序：不必同時升級，建議先升級 Pulse、再升級 Kairos。** 協定 v2 以版本協商相容 v1：新舊版本混用時自動以協定 v1 通訊（線上格式與舊版相同），Kairos 與 Pulse 可以分批、以任何順序升級，混用期間照常運作；兩端都是新版後才使用協定 v2（通用指標、主機資訊）。仍建議先升級 Pulse 的理由：

- 舊版 Pulse（`44dcee5` 及更早）連新版 Kairos 時，主機名稱含新版 Kairos 禁止的字元會**無法註冊**，心跳間隔較大時也可能誤斷線；新版 Pulse 連舊版 Kairos 沒有這類問題（舊版 Kairos 的 `timeouts.write_secs` 維持預設時）。先升級 Pulse 就不會經過有風險的組合。
- 資料格式的改變與順序無關：Kairos 升級後 JSONL 每行多了 `schema`，新版 Pulse 連上後寫成 `schema` 2，讀取 JSONL 的工具要在升級 Kairos **前**改為依 `schema` 分流（`CHANGELOG.md` 破壞性變更第 6 項）。

確認協商結果：Pulse 日誌的「已向伺服器註冊（…，協定 vN）」，連舊版 Kairos 時為 v1、連新版 Kairos 時為 v2。各版本組合的細節見 `CHANGELOG.md`「版本相容矩陣」與 `README.md`「升級注意事項」。

```bash
# 1. 下載新版的發佈包、SHA256SUMS 與 SHA256SUMS.sig，依第 2 節驗證簽章與雜湊
#    （allowed_signers 沿用第一次安裝時從另一個管道取得的那份），解壓到新的目錄
cd ~/pulse-dist
ssh-keygen -Y verify -f allowed_signers -I release@kairos-pulse -n kairos-pulse-release \
  -s SHA256SUMS.sig < SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
rm -rf pkg-new && mkdir pkg-new
tar xzf pulse-<新版本>-<新commit>-linux-x86_64.tar.gz -C pkg-new --strip-components=1
pkg-new/pulse --version

# 2. 比對預設值與 unit 是否有變動（pulse.toml 只寫覆蓋值，通常不必改）
diff /opt/pulse/pulse.example.toml pkg-new/pulse.example.toml
diff /etc/systemd/system/pulse.service pkg-new/pulse.service

# 3. 保留舊版以便回滾，換上新版
cp -p /opt/pulse/pulse /opt/pulse/pulse.prev
install -o root -g root -m 755 pkg-new/pulse /opt/pulse/pulse.new
mv /opt/pulse/pulse.new /opt/pulse/pulse
install -o root -g root -m 644 pkg-new/pulse.example.toml pkg-new/INSTALL.md \
  pkg-new/README.md pkg-new/PROTOCOL.md pkg-new/CHANGELOG.md pkg-new/THIRD-PARTY-LICENSES /opt/pulse/
# unit 有變動時：
# install -o root -g root -m 644 pkg-new/pulse.service /etc/systemd/system/pulse.service
# （多實例時，再依第 9 節步驟 2 重新產生每個 pulse-N.service）
# systemctl daemon-reload

# 4. 重新啟動並確認（第 8 節）
systemctl restart pulse          # 多實例時：systemctl restart 'pulse*.service'
/opt/pulse/pulse --version
tail -n 20 /opt/pulse/logs/pulse-$(date -u +%F).log
```

- `agent.id`、憑證與 `pulse.toml` 沿用，不需要重新簽發。
- 依舊版 INSTALL 安裝（`client.key`、`agent.id` 為 `pulse:pulse 600`）者，建議改成服務帳號唯讀的配置（多實例時對每個 `/opt/pulse-N` 執行相同指令）：

  ```bash
  chown root:pulse /opt/pulse/agent.id /opt/pulse/client.key
  chmod 640 /opt/pulse/agent.id /opt/pulse/client.key
  ```
- 升級前請看新版發佈包中的 `CHANGELOG.md`「破壞性變更與升級步驟」：例如設定了 `ca_cert` 時預設不再信任作業系統信任庫，伺服器憑證若由公開 CA 簽發，需在 `pulse.toml` 加上 `use_system_roots = true`；預設心跳由 1 秒改為 15 秒，Kairos 的離線判定由 5 秒變成 75 秒，需要舊行為時在 `pulse.toml` 設定 `[heartbeat] interval = 1`。
- 新版 unit 新增 `RestartPreventExitStatus=2 3 4`（用法、設定、憑證錯誤時不再每 10 秒重試）；請依步驟 2 換上新版 unit（多實例時重新產生每個 `pulse-N.service`）。
- 從沒有 `NotifyAccess=main` 的舊版 unit 升級時，步驟 2 的 `diff` 會列出這一行；請換上新版 unit（多實例時重新產生每個 `pulse-N.service`），否則 `systemctl status` 不會顯示 `Status:` 行（見 8.1）。
- 全部 Pulse 都升級並重新註冊後，再通知 Kairos 管理者升級 Kairos。
- 回滾：`mv /opt/pulse/pulse.prev /opt/pulse/pulse && systemctl restart pulse`。

## 11. 憑證更新與 agent_id 重新產生

- **憑證快到期**（日誌出現「剩餘不超過 N 天」的 `WARN`）：沿用同一個 agent_id，重做 5.2（新私鑰與 CSR，私鑰同樣設為 `root:pulse 640`）請管理者簽發，替換 `client.key`、`client.pem` 後 `systemctl restart pulse`。
- **重新產生 agent_id**（例如複製 VM 後）：`systemctl stop pulse`，刪除 `/opt/pulse/agent.id`，重做第 5 節全部步驟，並請管理者撤銷舊憑證。只換 `agent.id` 不換憑證時，Pulse 啟動即因 SAN 不符而結束。

## 12. 移除

在 root shell 中執行。有多實例（第 9 節）時，**先移除所有 `pulse-N`，最後才移除 `/opt/pulse` 與使用者**：`pulse-N` 共用 `/opt/pulse/pulse` 與使用者 `pulse`，先刪掉它們會讓殘留的 `pulse-N.service` 以 `Restart=always` 每 10 秒啟動失敗一次。

```bash
# 1. 記下每個實例的 agent_id，交給 Kairos 管理者撤銷憑證
grep -H . /opt/pulse/agent.id /opt/pulse-[0-9]*/agent.id 2>/dev/null

# 2. 停用並移除所有多實例 pulse-N（沒有多實例時不會做任何事）
for u in $(systemctl list-unit-files 'pulse-*.service' --no-legend | awk '{print $1}'); do
  systemctl disable --now "$u"
  rm -f "/etc/systemd/system/$u"
  rm -rf "/etc/systemd/system/$u.d"      # systemctl edit 建立的 drop-in
done

# 3. 停用並移除第一個實例
systemctl disable --now pulse
rm -f /etc/systemd/system/pulse.service
rm -rf /etc/systemd/system/pulse.service.d

# 4. 讓 systemd 忘掉已刪除的 unit（含失敗狀態）
systemctl daemon-reload
systemctl reset-failed 'pulse.service' 'pulse-*.service' 2>/dev/null

# 5. 最後才刪除目錄與使用者（先刪第 5、9 節放在 /tmp 的 CSR 副本，需要 agent.id 才找得到）
for id in $(cat /opt/pulse/agent.id /opt/pulse-[0-9]*/agent.id 2>/dev/null); do
  rm -f "/tmp/$id.csr"
done
rm -rf /opt/pulse-[0-9]*   # 多實例的工作目錄（含私鑰、agent.id 與日誌）
rm -rf /opt/pulse          # 含共用執行檔、客戶端私鑰、agent.id 與日誌
userdel pulse              # 同名群組 pulse 會一併移除

# 6. 確認沒有殘留
systemctl list-units --all 'pulse*' --no-legend      # 應無輸出
systemctl list-unit-files 'pulse*' --no-legend       # 應無輸出
ls -d /opt/pulse* 2>/dev/null                        # 應無輸出
```

請 Kairos 管理者撤銷每個實例的客戶端憑證（Kairos 的 `INSTALL.md`「撤銷與定期更新 CRL」）。Kairos 的狀態表會在離線 24 小時後自動移除這些 agent。
