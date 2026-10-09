# Pulse 安裝指南（從發佈包安裝）

本文件給地端管理者使用：從預編譯的發佈包把 Pulse 安裝成**系統層級 systemd 服務**，連到中央伺服器 Kairos。功能、設定欄位與協定細節請見同目錄的 `README.md` 與 `PROTOCOL.md`。

> 不要用 `systemd-run --user` 或使用者層級的 unit 跑正式服務：沒有開啟 linger 時，最後一個登入 session 結束，user manager 會連同 Pulse 一起被停掉。

## 0. 發佈包內容

發佈包 `pulse-<commit>-linux-x86_64.tar.gz` 內含一層 `pulse-<commit>-linux-x86_64/` 目錄：

| 檔案 | 權限 | 說明 |
|---|---|---|
| `pulse` | 755 | 執行檔（x86_64、musl 靜態連結，不依賴系統函式庫） |
| `pulse.example.toml` | 644 | **內建預設值的參考**（見第 6 節），不需要複製或修改 |
| `pulse.service` | 644 | systemd unit 範本（單一實例） |
| `INSTALL.md` | 644 | 本文件 |
| `README.md`、`PROTOCOL.md` | 644 | 說明文件與協定規格 |

檔名中的 `<commit>` 是建置時的 git commit；檔名帶 `-dirty` 表示從有未提交變更的工作目錄建置，**不應**用於正式環境。

以下範例的路徑與名稱：

| 項目 | 值 |
|---|---|
| 安裝目錄 | `/opt/pulse`（擁有者 `root:pulse`，750） |
| 服務使用者 | `pulse`（系統帳號，不能登入） |
| 可寫目錄 | `/opt/pulse/logs`（擁有者 `pulse`） |
| Kairos | `monitor.example.com:8080`（換成 Kairos 管理者提供的位址與憑證名稱） |

指令以 root 執行（或在前面加 `sudo`）。

## 1. 需求

- Linux x86_64、systemd（建議 247 以上）
- 能以 TCP 連到 Kairos 的監聽埠
- 產生私鑰與 CSR 用的 OpenSSL（1.1.1 以上即可；沒有的話見第 5 節的替代做法）
- 從 Kairos 管理者取得：Kairos 的位址與 `server.domain`（伺服器憑證 SAN 中的名稱）、伺服器 CA 憑證 `ca.pem`；啟用 mTLS 時還要由對方簽發客戶端憑證

## 2. 下載與驗證

```bash
mkdir -p ~/pulse-dist && cd ~/pulse-dist
# 取得 pulse-<commit>-linux-x86_64.tar.gz 與 SHA256SUMS（放在同一個目錄）
sha256sum -c SHA256SUMS                  # 必須顯示「…: OK」，否則不要解壓
```

`SHA256SUMS` 若同時列了 Kairos 的發佈包而你只下載了 Pulse，改用 `sha256sum -c --ignore-missing SHA256SUMS`。

## 3. 解壓並安裝執行檔

```bash
cd ~/pulse-dist
mkdir -p pkg
tar xzf pulse-<commit>-linux-x86_64.tar.gz -C pkg --strip-components=1
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
install -o root -g root -m 644 pulse.example.toml INSTALL.md README.md PROTOCOL.md /opt/pulse/
```

`/opt/pulse` 本身屬於 root，pulse 使用者只能讀取執行檔、設定與憑證；unit 以 `ProtectSystem=strict` 讓服務只能寫 `logs/`。**`logs/` 必須在啟動前存在**，否則 unit 以 `226/NAMESPACE` 啟動失敗。

## 5. 產生 agent_id 並申請客戶端憑證

### 5.1 產生 agent_id

每個 Pulse 實例有一個固定的 `agent_id`（UUID v4），存在工作目錄的 `agent.id`。服務執行時 `/opt/pulse` 是唯讀的，所以**安裝時先產生**：

```bash
cd /opt/pulse                         # 必須在實際的工作目錄執行，才會寫到同一份 agent.id
./pulse --print-agent-id              # stdout 印出 agent_id（小寫），「已產生新的 agent_id」提示在 stderr
chown pulse:pulse agent.id
chmod 600 agent.id
AGENT_ID=$(./pulse --print-agent-id)  # 之後的步驟會用到
echo "$AGENT_ID"
```

- `agent.id` 已存在時只會讀取，不會改變。
- **不要**把 `agent.id` 複製到其他主機（例如複製 VM 映像檔後忘了刪）：兩台會互相把對方踢下線（`superseded`）。複製出來的主機請刪除 `agent.id` 後重做本節。
- 未啟用 mTLS 時，`agent.id` 的內容就是本機在伺服器上的身分，權限必須是 600。

### 5.2 產生私鑰與 CSR，交給 Kairos 管理者簽發

私鑰在 Pulse 主機上產生，**不離開這台主機**；只把 CSR 與 agent_id 交給 Kairos 管理者：

```bash
cd /opt/pulse
umask 077
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout client.key -out "$AGENT_ID.csr" -subj "/CN=$AGENT_ID" \
  -addext "subjectAltName=DNS:$AGENT_ID" \
  -addext "extendedKeyUsage=clientAuth"
chown pulse:pulse client.key
chmod 600 client.key
```

- 也可以用 RSA：把 `-newkey ec -pkeyopt ec_paramgen_curve:P-256` 換成 `-newkey rsa:2048`。私鑰不可加密（`-nodes`）。
- `-addext` 需要 OpenSSL 1.1.1 以上；更舊的版本請改用下面的替代做法。
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
ca_cert = "ca.pem"                # 伺服器 CA
use_system_roots = false          # 只信任 ca_cert
client_cert = "client.pem"        # mTLS（5.3 的情況下刪掉這兩行）
client_key = "client.key"
```

相對路徑以工作目錄 `/opt/pulse` 解析。心跳、回報間隔等其他欄位維持預設即可（見 `README.md`「設定」）。

### 6.1 檔案權限

```bash
install -o root -g pulse -m 644 ca.pem     /opt/pulse/ca.pem       # 取回的檔案所在目錄執行
install -o root -g pulse -m 644 client.pem /opt/pulse/client.pem
```

| 路徑 | 擁有者 | 權限 |
|---|---|---|
| `/opt/pulse` | `root:pulse` | 750 |
| `pulse`（執行檔） | `root:root` | 755 |
| `pulse.toml` | `root:pulse` | 640 |
| `ca.pem`、`client.pem` | `root:pulse` | 644 |
| `client.key`、`agent.id` | `pulse:pulse` | 600 |
| `logs/` | `pulse:pulse` | 750 |

`$AGENT_ID.csr` 交出去後可以刪除。

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
| `Restart` | `always` | Pulse 本身會無限重連；取樣任務異常或其他原因結束時一律由 systemd 重啟 |
| `ProtectSystem=strict` + `ReadWritePaths=/opt/pulse/logs` | | 其餘檔案系統唯讀；因此 `agent.id` 必須在安裝時產生（第 5.1 節） |

### 7.1 為什麼選擇 `StandardOutput=null`

Pulse 把日誌同時寫到 stdout 與 `logs/pulse-YYYY-MM-DD.log`。systemd 預設把 stdout 收進 journal，結果同一份日誌存兩份。本範本選擇只保留 `logs/`：

- `logs/` 依 UTC 日期分檔，並由 Pulse 依 `log.retention_days`（預設 14 天）自動清除，空間可預期；journal 的保留則取決於整台主機的 journald 設定，大量 agent 主機上不易個別控制。
- 檔案格式與 Windows 等其他平台相同，蒐集與排查的方式一致，也與 Kairos 端的 `logs/` 對得起來。
- stderr 仍進 journal（`StandardError=journal`）：設定錯誤、日誌檔無法建立、panic 這類**日誌檔還沒開好就發生**的錯誤都寫在 stderr，所以啟動失敗時 `journalctl -u pulse` 仍看得到原因。

想改成只用 journal：以 `systemctl edit pulse` 加上 `[Service]`、`StandardOutput=journal`，並視需要把 `log.retention_days` 調小（`logs/` 仍會寫入，Pulse 沒有關閉檔案日誌的設定）。

## 8. 啟動與確認

```bash
systemctl enable --now pulse
systemctl status pulse --no-pager
```

確認清單：

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
[INFO] 已向伺服器註冊（agent_id=…，伺服器離線判定 5000 ms；TLS 加密連線 monitor.example.com:8080，domain=monitor.example.com，TLSv1_3，已驗證伺服器憑證）
```

- 判斷連線成功以「已向伺服器註冊」這一行為準。TLS 握手完成（「已建立 TLS 加密連線」）只在 `debug` 等級記錄，因為 TLS 1.3 下伺服器在握手後才驗證客戶端憑證，握手完成不代表會被接受。
- 這一行若是 `WARN` 且含「⚠️ 未驗證伺服器憑證」，表示設定了 `accept_invalid_certs = true`，正式環境不可如此。
- 啟動時的系統資訊在 `info` 等級只列實體磁碟與網路介面（略過虛擬檔案系統、容量 0 的掛載點、`lo` 與 `veth*`、`docker*`、`br-*` 等虛擬介面，各以一行統計），CPU 只有一行摘要；完整清單需 `log.level = "debug"`。

最後請 Kairos 管理者確認 Kairos 主機上的 `/opt/kairos/data/agents.json` 有這個 agent_id，且 `"online": true`。

常見的失敗原因：

| 日誌 | 處理 |
|---|---|
| `設定檔：/opt/pulse/pulse.toml 不存在，只使用內建預設值…` | 檔名或位置錯誤。檔案存在但 pulse 使用者沒有讀取權限時，Pulse 會以「無法載入設定」結束，原因在 `journalctl -u pulse` |
| `TCP 連線失敗` 持續出現 | 位址、連接埠或防火牆；Kairos 預設只聽 127.0.0.1 |
| `伺服器憑證的名稱與 server.domain「…」不符` | `server.domain` 要填伺服器憑證 SAN 中的名稱 |
| `伺服器憑證不受信任` | `ca_cert` 不是簽發 Kairos 伺服器憑證的 CA |
| 啟動即結束：`SAN 不含 DNS:<agent_id>` | 憑證不是用這台的 agent_id 簽發（例如 `agent.id` 重新產生過），重做 5.2 |
| `伺服器表示此客戶端憑證已被撤銷` | 憑證已被撤銷，請管理者重新簽發 |
| `無法初始化日誌`（journal） | `logs/` 不存在或不屬於 pulse |

其他訊息見 `README.md`「啟用 mTLS」與「重連行為」。

## 9. 多實例（同一台主機跑多個 Pulse）

一般一台主機只跑一個 Pulse。測試等情況需要多個實例時，每個實例必須有**自己的工作目錄、`agent.id`、客戶端憑證與 `logs/`**（共用 `agent.id` 會互相踢下線）。

做法：複製一份 unit，把所有 `/opt/pulse` 換成新目錄。執行檔可以共用 `/opt/pulse/pulse`：

```bash
N=2
install -d -o root  -g pulse -m 750 /opt/pulse-$N
install -d -o pulse -g pulse -m 750 /opt/pulse-$N/logs

# unit：工作目錄、可寫目錄改成 /opt/pulse-N，ExecStart 仍指向共用的執行檔
sed -e "s#/opt/pulse#/opt/pulse-$N#g" \
    -e "s#^ExecStart=.*#ExecStart=/opt/pulse/pulse#" \
    /etc/systemd/system/pulse.service > /etc/systemd/system/pulse-$N.service

# 每個實例各自產生 agent_id、私鑰與 CSR（第 5 節，在 /opt/pulse-N 執行）
cd /opt/pulse-$N && /opt/pulse/pulse --print-agent-id && chown pulse:pulse agent.id && chmod 600 agent.id
# …簽發憑證、建立 pulse.toml（第 5、6 節）…

systemctl daemon-reload
systemctl enable --now pulse-$N
```

多個實例都從同一個 IP 連入 Kairos，Kairos 端可能需要調高 `server.max_connections_per_ip`（見 Kairos 的 `README.md`）。

## 10. 升級

**升級順序：先升級全部 Pulse，再升級 Kairos**（或同時升級）。新版 Pulse 會替換新版 Kairos 禁止的主機名稱字元，連舊版 Kairos 不受影響；反過來，舊版 Pulse 連新版 Kairos 可能無法註冊（細節見 `README.md`「升級注意事項」）。

```bash
# 1. 下載並驗證新版，解壓到新的目錄
cd ~/pulse-dist && sha256sum -c SHA256SUMS
rm -rf pkg-new && mkdir pkg-new
tar xzf pulse-<新commit>-linux-x86_64.tar.gz -C pkg-new --strip-components=1
pkg-new/pulse --version

# 2. 比對預設值與 unit 是否有變動（pulse.toml 只寫覆蓋值，通常不必改）
diff /opt/pulse/pulse.example.toml pkg-new/pulse.example.toml
diff /etc/systemd/system/pulse.service pkg-new/pulse.service

# 3. 保留舊版以便回滾，換上新版
cp -p /opt/pulse/pulse /opt/pulse/pulse.prev
install -o root -g root -m 755 pkg-new/pulse /opt/pulse/pulse.new
mv /opt/pulse/pulse.new /opt/pulse/pulse
install -o root -g root -m 644 pkg-new/pulse.example.toml pkg-new/INSTALL.md \
  pkg-new/README.md pkg-new/PROTOCOL.md /opt/pulse/
# unit 有變動時：
# install -o root -g root -m 644 pkg-new/pulse.service /etc/systemd/system/pulse.service
# systemctl daemon-reload

# 4. 重新啟動並確認（第 8 節）
systemctl restart pulse          # 多實例時：systemctl restart 'pulse*.service'
/opt/pulse/pulse --version
tail -n 20 /opt/pulse/logs/pulse-$(date -u +%F).log
```

- `agent.id`、憑證與 `pulse.toml` 沿用，不需要重新簽發。
- 全部 Pulse 都升級並重新註冊後，再通知 Kairos 管理者升級 Kairos。
- 回滾：`mv /opt/pulse/pulse.prev /opt/pulse/pulse && systemctl restart pulse`。

## 11. 憑證更新與 agent_id 重新產生

- **憑證快到期**（日誌出現「剩餘不超過 N 天」的 `WARN`）：沿用同一個 agent_id，重做 5.2（新私鑰與 CSR）請管理者簽發，替換 `client.key`、`client.pem` 後 `systemctl restart pulse`。
- **重新產生 agent_id**（例如複製 VM 後）：`systemctl stop pulse`，刪除 `/opt/pulse/agent.id`，重做第 5 節全部步驟，並請管理者撤銷舊憑證。只換 `agent.id` 不換憑證時，Pulse 啟動即因 SAN 不符而結束。

## 12. 移除

```bash
systemctl disable --now pulse
rm /etc/systemd/system/pulse.service
systemctl daemon-reload
systemctl reset-failed pulse 2>/dev/null

rm -rf /opt/pulse          # 含客戶端私鑰、agent.id 與日誌
userdel pulse
```

請 Kairos 管理者撤銷這台的客戶端憑證（Kairos 的 `INSTALL.md`「撤銷與定期更新 CRL」）。Kairos 的狀態表會在離線 24 小時後自動移除這個 agent。
