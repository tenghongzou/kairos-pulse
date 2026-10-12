# Kairos 安裝指南（從發佈包安裝）

本文件給地端管理者使用：從預編譯的發佈包把 Kairos 安裝成**系統層級 systemd 服務**。功能、設定欄位與協定細節請見同目錄的 `README.md` 與 `PROTOCOL.md`。

> 不要用 `systemd-run --user` 或使用者層級的 unit 跑正式服務：沒有開啟 linger 時，最後一個登入 session 結束，user manager 會連同 Kairos 一起被停掉。

## 0. 發佈包內容

發佈包 `kairos-<版本>-<commit>-linux-x86_64.tar.gz` 內含一層 `kairos-<版本>-<commit>-linux-x86_64/` 目錄：

| 檔案 | 權限 | 說明 |
|---|---|---|
| `kairos` | 755 | 執行檔（x86_64、musl 靜態連結，不依賴系統函式庫） |
| `kairos.example.toml` | 644 | 設定範本，複製成 `kairos.toml` 後修改 |
| `kairos.service` | 644 | systemd unit 範本 |
| `kairos-crl.service`、`kairos-crl.timer` | 644 | CRL 定期重新產生與部署的 systemd 範例（第 10.3 節） |
| `INSTALL.md` | 644 | 本文件 |
| `README.md`、`PROTOCOL.md` | 644 | 說明文件與協定規格 |
| `CHANGELOG.md` | 644 | 變更紀錄；升級前請先看「破壞性變更與升級步驟」 |
| `THIRD-PARTY-LICENSES` | 644 | 執行檔內含的第三方套件清單與授權全文（建置時由 `cargo about` 依 `Cargo.lock` 產生） |
| `kairos.cdx.json` | 644 | SBOM（CycloneDX 1.5 JSON，建置時由 `cargo cyclonedx` 產生）；發佈處另附一份同內容的 `kairos-<版本>-<commit>-linux-x86_64.cdx.json`，可直接匯入 Dependency-Track 等工具 |

檔名中的 `<版本>` 是 SemVer 版本號（與 `--version` 相同，版本規則見 `CHANGELOG.md`），`<commit>` 是建置時的 git commit；檔名帶 `-dirty` 表示從有未提交變更的工作目錄建置，**不應**用於正式環境。

以下範例的路徑與名稱：

| 項目 | 值 |
|---|---|
| 安裝目錄 | `/opt/kairos`（擁有者 `root:kairos`，750） |
| 服務使用者 | `kairos`（系統帳號，不能登入） |
| 可寫目錄 | `/opt/kairos/data`、`/opt/kairos/logs`（擁有者 `kairos`） |
| 監聽位址 | `0.0.0.0:8080`（與 Pulse 預設的 `server.port` 相同） |
| 伺服器名稱 | `monitor.example.com`、`10.0.0.5`（換成實際值） |
| PKI 工作目錄 | `~/kairos-pki`（**放在離線或受保護的管理主機**，不要放在 Kairos 主機上） |

指令以 root 執行（或在前面加 `sudo`）。

## 1. 需求

- Linux x86_64、systemd（建議 247 以上；Ubuntu 22.04／24.04、Debian 12、RHEL 9 皆可）
- 管理主機上的 OpenSSL 3.0 以上（建立 PKI；`openssl version` 確認）
- 驗證發佈包簽章用的 OpenSSH 8.1 以上（`ssh -V`；可在管理主機上驗證，見第 2 節）
- 防火牆允許 Pulse 連到監聽埠（本例 TCP 8080）

## 2. 下載與驗證

從發佈處取得下列檔案，放在同一個目錄：

| 檔案 | 說明 |
|---|---|
| `kairos-<版本>-<commit>-linux-x86_64.tar.gz` | 發佈包 |
| `SHA256SUMS` | Kairos 與 Pulse 共用的雜湊清單（同一次發佈的所有檔案） |
| `SHA256SUMS.sig` | `SHA256SUMS` 的簽章（`ssh-keygen -Y sign`） |
| `allowed_signers` | 發佈公鑰。**必須從另一個管道取得**，見下方說明 |

```bash
mkdir -p ~/kairos-dist && cd ~/kairos-dist
# 1. 驗證 SHA256SUMS 的簽章：必須顯示
#    Good "kairos-pulse-release" signature for release@kairos-pulse with ED25519 key SHA256:…
ssh-keygen -Y verify -f allowed_signers -I release@kairos-pulse -n kairos-pulse-release \
  -s SHA256SUMS.sig < SHA256SUMS
# 2. 驗證發佈包的雜湊：必須顯示「kairos-…tar.gz: OK」
sha256sum -c --ignore-missing SHA256SUMS
```

任一步失敗（`Could not verify signature`、`FAILED`）都不要解壓，請向供應商確認。`--ignore-missing` 是因為 `SHA256SUMS` 也列了 Pulse 的發佈包與 SBOM；只要你下載的發佈包顯示 `OK` 即可。

**公鑰必須從另一個管道取得**：簽章只能證明 `SHA256SUMS` 由持有發佈私鑰的人簽署。若 `allowed_signers` 和發佈包從同一個地方下載，能替換發佈包的人也能一起替換公鑰，驗證就沒有意義。請從供應商另外提供的管道取得 `allowed_signers`（例如合約或交付文件、客戶入口網站的 HTTPS 頁面），第一次使用時再以電話等已知可信的方式核對指紋；之後保存在管理主機上重複使用，供應商更換金鑰時會另行通知。

```bash
awk '{print $3, $4}' allowed_signers | ssh-keygen -lf -   # 印出 256 SHA256:… (ED25519)，與供應商提供的指紋比對
```

`ssh-keygen -Y verify` 需要 OpenSSH 8.1 以上（`ssh -V`）。RHEL／Rocky 8（OpenSSH 8.0）、Debian 10 等較舊的系統沒有這個功能，請在管理主機（Windows 11 內建的 OpenSSH 或 Git for Windows 的 `ssh-keygen`、macOS、Ubuntu 20.04 以上、Debian 11 以上、RHEL 9）完成驗證後，再把發佈包複製到目標主機。

## 3. 解壓

```bash
cd ~/kairos-dist
mkdir -p pkg
tar xzf kairos-<版本>-<commit>-linux-x86_64.tar.gz -C pkg --strip-components=1
file pkg/kairos              # 應為 ELF 64-bit x86-64 static-pie linked
pkg/kairos --version         # 輸出 kairos <版本> (<commit>)，commit 應與檔名一致
```

tar 內有一層目錄，所以要加 `--strip-components=1`。

## 4. 建立使用者與目錄

```bash
useradd --system --user-group --home-dir /opt/kairos --no-create-home \
  --shell /usr/sbin/nologin kairos

install -d -o root   -g kairos -m 750 /opt/kairos
install -d -o kairos -g kairos -m 750 /opt/kairos/data /opt/kairos/logs
```

`/opt/kairos` 本身屬於 root，kairos 使用者只能讀取執行檔、設定與憑證，不能修改；只有 `data/`、`logs/` 可寫。unit 也以 `ProtectSystem=strict` 讓服務只能寫這兩個目錄，**兩個目錄必須在啟動前存在**，否則 unit 以 `226/NAMESPACE` 啟動失敗。

## 5. 安裝執行檔與文件

```bash
cd ~/kairos-dist/pkg
install -o root -g root -m 755 kairos /opt/kairos/kairos
install -o root -g root -m 644 kairos.example.toml INSTALL.md README.md PROTOCOL.md \
  CHANGELOG.md THIRD-PARTY-LICENSES /opt/kairos/
```

## 6. 建立 PKI

正式環境需要兩個 CA，用途不同、請分開建立：

| CA | 用途 | 管理方式 |
|---|---|---|
| 伺服器 CA（`ca.pem`） | 簽發 Kairos 的伺服器憑證；每台 Pulse 只信任它 | `openssl x509 -req`（伺服器憑證不需撤銷，到期重簽即可） |
| 代理 CA（`client-ca.pem`） | **只**用來簽發 Pulse 的客戶端憑證（mTLS） | `openssl ca` 資料庫，才能撤銷並產生 CRL |

兩把 CA 私鑰（`ca.key`、`client-ca.key`）都在管理主機上產生並離線保存，**不要**複製到 Kairos 主機。以下指令在管理主機執行。

### 6.1 伺服器 CA 與伺服器憑證

```bash
mkdir -p ~/kairos-pki/server-ca && cd ~/kairos-pki/server-ca
umask 077

# 伺服器 CA（只需一次）
openssl req -x509 -newkey rsa:4096 -nodes \
  -keyout ca.key -out ca.pem -days 3650 -subj "/CN=Kairos Server CA" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign"

# 伺服器私鑰與 CSR：SAN 填 Pulse 連線時使用的所有名稱／IP
openssl req -new -newkey rsa:2048 -nodes \
  -keyout key.pem -out server.csr -subj "/CN=monitor.example.com" \
  -addext "subjectAltName=DNS:monitor.example.com,IP:10.0.0.5" \
  -addext "basicConstraints=critical,CA:FALSE" \
  -addext "extendedKeyUsage=serverAuth"

# 以伺服器 CA 簽發（-copy_extensions copyall 需 OpenSSL 3.0+，才會帶入 SAN）
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
  -out cert.pem -days 825 -copy_extensions copyall

openssl x509 -in cert.pem -noout -ext subjectAltName,basicConstraints,extendedKeyUsage
```

- `cert.pem`、`key.pem` 交給 Kairos（第 7 節）。
- `ca.pem` 交給每台 Pulse（`server.ca_cert`），Pulse 的 `server.domain` 填 SAN 中的名稱。
- `basicConstraints=critical,CA:FALSE` 不能省：rustls 不接受 CA 憑證當伺服器憑證（`CaUsedAsEndEntity`）。

### 6.2 代理 CA（openssl ca 資料庫，可撤銷）

```bash
mkdir -p ~/kairos-pki/agent-ca/newcerts && cd ~/kairos-pki/agent-ca
umask 077
touch index.txt
echo 1000 > serial
echo 1000 > crlnumber

cat > ca.cnf <<'EOF'
# 簽發時以環境變數 AGENT_ID 指定代理的 agent_id；未設定時簽發會失敗（避免簽出錯誤的 SAN）
AGENT_ID =

[ ca ]
default_ca = agent_ca

[ agent_ca ]
# 路徑相對於「目前目錄」：所有 openssl ca 指令都必須在 agent-ca/ 目錄內執行
dir              = .
database         = $dir/index.txt
serial           = $dir/serial
crlnumber        = $dir/crlnumber
new_certs_dir    = $dir/newcerts
certificate      = $dir/client-ca.pem
private_key      = $dir/client-ca.key
default_md       = sha256
default_days     = 825
default_crl_days = 30
policy           = policy_any
# 不複製 CSR 中的擴充欄位：SAN 一律由簽發者以 AGENT_ID 指定，CSR 夾帶的其他 SAN 不會進入憑證
copy_extensions  = none
# 允許同一個 agent_id 重新簽發（換金鑰）而不必先撤銷舊憑證
unique_subject   = no

[ policy_any ]
commonName       = supplied

[ client_ext ]
basicConstraints       = critical,CA:FALSE
keyUsage               = critical,digitalSignature
extendedKeyUsage       = clientAuth
subjectAltName         = DNS:${ENV::AGENT_ID}
subjectKeyIdentifier   = hash
authorityKeyIdentifier = keyid,issuer

[ crl_ext ]
authorityKeyIdentifier = keyid:always
EOF

# 代理 CA 憑證（只需一次）
openssl req -x509 -newkey rsa:3072 -nodes \
  -keyout client-ca.key -out client-ca.pem -days 3650 -subj "/CN=Kairos Agent CA" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign"

# 第一份 CRL（即使還沒有撤銷任何憑證也要產生，Kairos 設定了 auth.crl 就必須讀得到）
openssl ca -batch -config ca.cnf -gencrl -crlexts crl_ext -out agents.crl
openssl crl -in agents.crl -noout -lastupdate -nextupdate
```

- `client-ca.pem` 與 `agents.crl` 交給 Kairos（第 7 節）；`client-ca.key` 與整個 `agent-ca/` 目錄離線保存並備份（`index.txt` 遺失就無法撤銷先前簽發的憑證）。
- 代理 CA 只用來簽發代理的客戶端憑證：Kairos 信任它簽發的**任何**客戶端憑證。

### 6.3 簽發代理的客戶端憑證

每台 Pulse 的管理者依 Pulse 的 `INSTALL.md` 在代理主機上產生 `agent_id`、私鑰與 CSR，把 **CSR 與 agent_id** 交給你（私鑰不離開代理主機）。

```bash
cd ~/kairos-pki/agent-ca
AGENT_ID=6f1c0e4a-1b2c-4d3e-9f00-112233445566     # 對方提供的 agent_id（小寫 UUID）
mkdir -p issued

# 先檢視 CSR：確認 CN／SAN 與 agent_id 一致（不一致時向對方確認，不要直接簽）
openssl req -in "$AGENT_ID.csr" -noout -verify -subject
openssl req -in "$AGENT_ID.csr" -noout -text | grep -A1 "Subject Alternative Name"

# 簽發：SAN 強制為 DNS:<AGENT_ID>，CN 強制為 agent_id
AGENT_ID="$AGENT_ID" openssl ca -batch -config ca.cnf -extensions client_ext \
  -subj "/CN=$AGENT_ID" -in "$AGENT_ID.csr" -out "issued/$AGENT_ID.pem" -notext

openssl x509 -in "issued/$AGENT_ID.pem" -noout -serial -enddate \
  -ext subjectAltName,extendedKeyUsage
```

把 `issued/$AGENT_ID.pem`（對方存成 `client.pem`）與伺服器 CA 的 `ca.pem` 交回給該台代理。記下序號與 agent_id 的對應（`index.txt` 也有紀錄），之後撤銷時要用。

### 6.4 撤銷與定期更新 CRL

```bash
cd ~/kairos-pki/agent-ca
openssl ca -batch -config ca.cnf -revoke "issued/$AGENT_ID.pem" -crl_reason keyCompromise
openssl ca -batch -config ca.cnf -gencrl -crlexts crl_ext -out agents.crl
```

把新的 `agents.crl` 部署到 Kairos（第 10.1 節）。Kairos 在下一個重載週期（`server.reload_interval_secs`，預設 30 秒）載入，被撤銷的代理收到 `certificate_rejected` 並被斷線，之後重連在 TLS 握手階段就被拒；其他代理不受影響。

> **CRL 過期會讓所有代理被拒。** Kairos 對 CRL 採 fail closed：超過 `nextUpdate`（上面設定為 30 天）就視為無法判定撤銷狀態。請在到期前定期重新產生並部署，例如每週一次（即使沒有新的撤銷；第 10.3 節的 `kairos-crl.timer`）。`openssl crl -in agents.crl -noout -nextupdate` 可查看到期時間。Kairos 在 `nextUpdate` 前 7 天（`auth.crl_expiry_warning_days`）起記 `WARN`，過期後記 `ERROR`。

## 7. 放置設定與憑證

把第 6 節產生的檔案帶到 Kairos 主機（例如 `scp` 到 `~/kairos-files/`），再放到 `/opt/kairos`：

```bash
cd ~/kairos-files
install -o root -g kairos -m 644 cert.pem       /opt/kairos/cert.pem
install -o root -g kairos -m 640 key.pem        /opt/kairos/key.pem
install -o root -g kairos -m 644 client-ca.pem  /opt/kairos/client-ca.pem
install -o root -g kairos -m 644 agents.crl     /opt/kairos/agents.crl

install -o root -g kairos -m 640 /opt/kairos/kairos.example.toml /opt/kairos/kairos.toml
```

編輯 `/opt/kairos/kairos.toml`（完整欄位見 `README.md`「設定」），至少修改：

```toml
[server]
listen = "0.0.0.0:8080"          # 預設只聽 127.0.0.1，其他主機連不進來
# max_connections = 1024         # 調高時，unit 的 LimitNOFILE 也要 > max_connections + 64

[auth]
client_ca = "client-ca.pem"      # 啟用 mTLS（正式環境必須；監聽 0.0.0.0 卻沒有設定時 Kairos 拒絕啟動）
crl = ["agents.crl"]
```

`server.listen` 不是 loopback 位址（`127.0.0.0/8`、`::1`）又沒有設定 `auth.client_ca` 時，Kairos 會拒絕啟動並記錄 `ERROR`「未啟用客戶端憑證驗證（mTLS）卻監聽非 loopback 位址，拒絕啟動」。`auth.allow_unauthenticated = true` 可以略過這項檢查，但任何能連線的人都能以任意 `agent_id` 註冊，**不可用於正式環境**。

相對路徑以工作目錄 `/opt/kairos` 解析，`cert.pem`、`key.pem`、`data/`、`logs/` 維持預設即可。

檔案權限總覽：

| 路徑 | 擁有者 | 權限 |
|---|---|---|
| `/opt/kairos` | `root:kairos` | 750 |
| `kairos`（執行檔） | `root:root` | 755 |
| `kairos.toml`、`key.pem` | `root:kairos` | 640（`key.pem` 由 root 擁有、只讓 `kairos` 群組讀取：服務帳號讀得到但不能改寫或替換） |
| `cert.pem`、`client-ca.pem`、`agents.crl` | `root:kairos` | 644 |
| `data/`、`logs/` | `kairos:kairos` | 750 |
| `data/` 中的 `reports-*.jsonl`、`agents.json`；`logs/` 中的 `kairos-*.log` | `kairos:kairos` | 640（由 Kairos 建立） |

Kairos 自己建立的 report 檔、`agents.json` 與日誌檔一律最多 640，自行建立的目錄為 750，不受 umask 影響（手動執行時 umask 常為 022 也一樣）；既有檔案在開啟時收緊到最多 640（不會放寬，600 維持 600）。`data/`、`logs/` 已存在時 Kairos 不改變其權限，但其他使用者可存取（例如 755）時啟動會記 `WARN` 並建議 `chmod 750`。`agents.json` 與日期保護狀態檔（`.retention-guard*.json`，只含日期）的暫存檔建立時就是 640，更名後同樣最多 640。

## 8. 安裝 systemd unit

```bash
install -o root -g root -m 644 ~/kairos-dist/pkg/kairos.service /etc/systemd/system/kairos.service
systemctl daemon-reload
systemd-analyze verify /etc/systemd/system/kairos.service    # 沒有輸出表示格式正確
```

unit 重點（內容見 `kairos.service`）：

| 設定 | 值 | 理由 |
|---|---|---|
| `User`／`Group` | `kairos` | 不以 root 執行 |
| `WorkingDirectory` | `/opt/kairos` | 設定中的相對路徑都以此解析 |
| `ExecStart` | `/opt/kairos/kairos --config /opt/kairos/kairos.toml` | 明確指定設定檔，檔案不存在時啟動即報錯（不會默默使用預設值） |
| `LimitNOFILE` | `8192` | 需大於 `server.max_connections` + 64；Ubuntu 預設 soft limit 1024 在預設的 1024 條連線下會記 `WARN`。不足時 `WARN` 會附上建議值（預設設定下為 4096），8192 已足夠，並保留調高 `max_connections` 的餘裕 |
| `KillSignal`／`TimeoutStopSec` | `SIGINT`／`10` | Kairos 收到訊號後最壞約 3 秒正常關機（看門狗 4 秒） |
| `Restart` | `on-failure` | 背景工作異常時 Kairos 以結束代碼 1 結束，交由 systemd 重啟；連接埠被佔用（11，例如舊的行程還沒結束）也會重試；正常停止（結束代碼 0）不重啟 |
| `RestartPreventExitStatus` | `2 3 4` | 重試也不會成功的錯誤不重啟：2 命令列參數錯誤、3 設定錯誤（設定檔無法載入或驗證不通過、未啟用 mTLS 卻監聽非 loopback 位址）、4 憑證錯誤（伺服器憑證、私鑰、client CA、CRL 無法讀取、格式錯誤或不成對）。`systemctl status kairos` 會顯示 `status=2`／`3`／`4`，修正後 `systemctl restart kairos` |
| `ProtectSystem=strict` + `ReadWritePaths` | `data`、`logs` | 其餘檔案系統唯讀。熱重載只**讀取**憑證、私鑰、CA 與 CRL，管理者在主機上覆寫檔案後服務內立即可見，不受影響 |
| `ProtectHome`、`PrivateTmp`、`NoNewPrivileges` 等 | | 一般硬化；憑證與設定不可放在 `/home`、`/root` 之下 |

要調整設定時用 drop-in，不要直接改 unit 檔（升級時會被覆蓋）：

```bash
systemctl edit kairos
# 例如：監聽 443 等 1024 以下的連接埠
#   [Service]
#   AmbientCapabilities=CAP_NET_BIND_SERVICE
# 例如：只保留 logs/ 一份日誌，不寫 journal（見第 9.2 節）
#   [Service]
#   StandardOutput=null
```

## 9. 啟動與確認

```bash
systemctl enable --now kairos
systemctl status kairos --no-pager
```

### 9.1 確認清單

以下指令需要 root（`/opt/kairos` 與其下的 `logs/`、`data/` 只有 `kairos` 使用者與 root 可讀）：請在 `sudo -i` 的 shell 中執行，或在每行前加 `sudo`。

```bash
/opt/kairos/kairos --version                 # kairos <版本> (<commit>)
journalctl -u kairos -n 50 --no-pager        # 啟動日誌
ss -ltnp | grep kairos                       # 監聽 0.0.0.0:8080
tail -n 20 /opt/kairos/logs/kairos-$(date -u +%F).log
cat /opt/kairos/data/agents.json             # 每 5 秒更新；online 等於已連線的 Pulse 數
```

啟動成功時日誌會有：

```
[INFO] kairos <版本> (<commit>)
[INFO] 設定檔: /opt/kairos/kairos.toml
[INFO] 已啟用客戶端憑證驗證（mTLS），信任 client-ca.pem，CRL 1 份；agent_id 必須與憑證的 SAN 相符
[INFO] Kairos 已啟動，監聽 0.0.0.0:8080（連線上限 1024，…）
```

- `systemctl status kairos` 顯示 `(code=exited, status=3)` 且服務停在 `failed`、沒有重試：設定錯誤，用 `journalctl -u kairos -n 30` 看錯誤訊息，修正 `kairos.toml` 後 `systemctl restart kairos`。
- 顯示 `status=4` 且停在 `failed`：憑證錯誤（伺服器憑證、私鑰、`client-ca.pem`、CRL 讀不到、格式錯誤或憑證與私鑰不成對；常見原因是檔案權限或路徑），錯誤訊息在 `journalctl -u kairos -n 30` 與 `logs/`。修正後 `systemctl restart kairos`。`status=2` 為 `ExecStart` 的命令列參數錯誤。
- 顯示 `status=11` 並每 5 秒重試：`server.listen` 的連接埠被其他程式佔用（`ss -ltnp | grep :8080` 查看是誰）。
- 啟動失敗並出現 `未啟用客戶端憑證驗證（mTLS）卻監聽非 loopback 位址，拒絕啟動`：`kairos.toml` 沒有設定 `auth.client_ca`。依第 6、7 節啟用 mTLS。
- 出現 `未啟用客戶端憑證驗證（auth.client_ca）卻監聽非 loopback 位址 …（auth.allow_unauthenticated = true）` 的 `WARN`：設定了 `auth.allow_unauthenticated = true`，正式環境不可如此。
- 出現 `CRL … 將在 … 後過期` 或 `伺服器憑證 … 將在 … 後到期` 的 `WARN`（已過期時為 `ERROR`）：依第 10.1、10.3 節更新。
- 出現 `伺服器私鑰 … 的權限過寬` 的 `WARN`：`key.pem` 讓其他使用者有權限、group 可寫入，或 group 可讀但群組不是 `kairos`。依訊息執行 `chown root:kairos /opt/kairos/key.pem && chmod 640 /opt/kairos/key.pem`。Kairos 在啟動時與熱重載換了私鑰時檢查，只記錄不拒絕啟動。
- 出現 `可開啟的檔案數上限（RLIMIT_NOFILE soft limit）…` 的 `WARN`：unit 的 `LimitNOFILE` 沒有生效或小於 `server.max_connections` + 64；依訊息中的建議值以 drop-in 調高 `LimitNOFILE`（`systemctl show kairos -p LimitNOFILE` 可確認目前值）。
- 從其他主機確認 TLS 與伺服器憑證：`openssl s_client -connect monitor.example.com:8080 -CAfile ca.pem -servername monitor.example.com </dev/null`（啟用 mTLS 時，沒帶客戶端憑證會在握手後被拒，這是正常的）。
- Pulse 上線後，`agents.json` 的 `online` 會增加。多台 Pulse 經同一個 NAT 連入時，**以 `agents.json` 作為上線狀態的依據**：日誌的上線／離線事件有節流（見 `README.md`「日誌」）。

### 9.2 日誌放在哪裡

Kairos 的日誌同時寫到 stdout 與 `/opt/kairos/logs/kairos-YYYY-MM-DD.log`（UTC 日期分檔，依 `log.retention_days` 清除）。在預設的 unit 下 stdout 進 journal，所以同一份日誌會存兩份：

- 保留預設：可用 `journalctl -u kairos` 查詢，與 systemd 的啟停紀錄放在一起。
- 只要檔案一份：以 drop-in 設定 `StandardOutput=null`。啟動失敗等日誌初始化前的錯誤寫在 stderr，仍會進 journal。

## 10. 日常維運

### 10.1 更新 CRL、伺服器憑證、client CA（不需重新啟動）

先寫到同一目錄的暫存檔、設好權限，再以 `mv` 原子覆寫，避免 Kairos 讀到寫一半的檔案：

```bash
install -o root -g kairos -m 644 agents.crl /opt/kairos/agents.crl.new
mv /opt/kairos/agents.crl.new /opt/kairos/agents.crl

# 伺服器憑證：先換私鑰再換憑證（兩者短暫不成對時 Kairos 會記 ERROR 並保留舊設定，下個週期自動成功）
install -o root -g kairos -m 640 key.pem  /opt/kairos/key.pem.new
install -o root -g kairos -m 644 cert.pem /opt/kairos/cert.pem.new
mv /opt/kairos/key.pem.new /opt/kairos/key.pem
mv /opt/kairos/cert.pem.new /opt/kairos/cert.pem
```

在下一個重載週期內日誌會出現 `已重新載入 TLS 設定（變更：crl；mTLS：啟用，CRL 1 份）`（「變更」列出內容有變動的檔案種類：`cert`、`key`、`client_ca`、`crl`）。新檔案必須讓 kairos 使用者可讀；讀不到時記 `ERROR 重新載入 TLS 設定失敗，繼續使用目前的設定`，舊設定繼續生效。`kairos.toml` 本身的變更（例如監聽位址）需要 `systemctl restart kairos`。

### 10.2 備份

- `/opt/kairos/kairos.toml`、憑證與 CRL
- `/opt/kairos/data/`（report 的 JSONL）。搬移或備份時一併保留 `data/.retention-guard.json` 與 `logs/.retention-guard-logs.json`，不要刪除
- 管理主機的 `~/kairos-pki/`（含 CA 私鑰，離線保存）

### 10.3 定期更新 CRL（kairos-crl.timer）

發佈包附有 `kairos-crl.service`（重新產生 CRL、檢查後部署）與 `kairos-crl.timer`（每週執行一次）範例。安裝在**保存代理 CA 的主機**（第 6.2 節的 `agent-ca/`）上；`kairos-crl.service` 依序：

1. `openssl ca -gencrl` 產生到 `agents.crl.new`；
2. 確認由 `client-ca.pem` 簽發、`nextUpdate` 在未來；
3. 改名為 `agents.crl`，再部署到 Kairos。

任一步失敗時後面的步驟不會執行，Kairos 上的 CRL 維持原樣。

```bash
# 以下以代理 CA 放在 /srv/kairos-pki/agent-ca、由 kairos-pki 帳號擁有為例
install -m 644 ~/kairos-dist/pkg/kairos-crl.service ~/kairos-dist/pkg/kairos-crl.timer /etc/systemd/system/
systemctl edit kairos-crl.service      # 依實際環境覆寫 User、Group、WorkingDirectory 與部署方式
systemctl daemon-reload
systemctl start kairos-crl.service     # 先手動執行一次
journalctl -u kairos-crl -n 20 --no-pager
systemctl enable --now kairos-crl.timer
systemctl list-timers kairos-crl.timer
```

部署方式（`ExecStartPost`）二擇一：

- **CA 與 Kairos 在同一台主機**（範例預設）：以 root 執行 `install -o root -g kairos -m 644 … /opt/kairos/agents.crl.new` 再 `mv` 覆寫。代理 CA 的私鑰因此放在伺服器上，伺服器被入侵時攻擊者可以簽發代理憑證，請依組織政策評估。
- **CA 在另一台管理主機**：在 Kairos 主機建立部署帳號與只放 CRL 的目錄，讓部署帳號可寫、`kairos` 群組可讀，並把 `auth.crl` 指向該目錄：

  ```bash
  # Kairos 主機
  useradd --system --create-home --shell /bin/sh crl-deploy
  install -d -o crl-deploy -g kairos -m 750 /opt/kairos/crl
  install -o crl-deploy -g kairos -m 644 /opt/kairos/agents.crl /opt/kairos/crl/agents.crl
  # kairos.toml：[auth] crl = ["crl/agents.crl"]，之後 systemctl restart kairos
  # 管理主機的 kairos-pki 帳號以 SSH 金鑰登入 crl-deploy（建議在 authorized_keys 加上 from= 等限制）
  ```

  再以 drop-in 改用 `scp` 與 `ssh … mv`（`kairos-crl.service` 註解中有範例）。CRL 由 CA 簽章，傳輸途中被竄改時 Kairos 會拒絕載入並保留舊的 CRL（記 `ERROR`）。

更新間隔要遠小於 CRL 的有效期間（`ca.cnf` 的 `default_crl_days = 30` 時每週一次，可容許連續失敗 3 次）。排程失敗時 Kairos 會在 `nextUpdate` 前 7 天起記 `WARN`「CRL … 將在 … 後過期」，請以 `systemctl status kairos-crl.service` 檢查。

伺服器憑證到期前 30 天（`server.cert_expiry_warning_days`）起 Kairos 也會記 `WARN`，請依第 6.1 節重新簽發並依第 10.1 節覆寫。

## 11. 升級

**未啟用 mTLS 的部署請先處理**：從舊版升級時，若 `kairos.toml` 的 `server.listen` 是非 loopback 位址（例如 `0.0.0.0:8080`）而沒有設定 `auth.client_ca`，新版會拒絕啟動（舊版只記 `WARN`）。升級前先依第 6、7 節啟用 mTLS；只有隔離的測試環境才在 `[auth]` 加上 `allow_unauthenticated = true`。

**換上新版 unit**：新版 `kairos.service` 新增 `RestartPreventExitStatus=2 3 4`（用法、設定、憑證錯誤時不再無限重試）。升級時以發佈包的 `kairos.service` 覆寫 `/etc/systemd/system/kairos.service`（自訂內容放在 `systemctl edit` 的 drop-in）後 `systemctl daemon-reload`。完整的變更與升級步驟見新版發佈包中的 `CHANGELOG.md`。

**升級順序：先升級全部 Pulse，再升級 Kairos**（或同時升級）。新版 Pulse 會替換新版 Kairos 禁止的主機名稱字元，連舊版 Kairos 不受影響；反過來，舊版 Pulse 連新版 Kairos 可能無法註冊（細節見 `README.md`「升級注意事項」）。

```bash
# 1. 下載新版的發佈包、SHA256SUMS 與 SHA256SUMS.sig，依第 2 節驗證簽章與雜湊
#    （allowed_signers 沿用第一次安裝時從另一個管道取得的那份），解壓到新的目錄
cd ~/kairos-dist
ssh-keygen -Y verify -f allowed_signers -I release@kairos-pulse -n kairos-pulse-release \
  -s SHA256SUMS.sig < SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
rm -rf pkg-new && mkdir pkg-new
tar xzf kairos-<新版本>-<新commit>-linux-x86_64.tar.gz -C pkg-new --strip-components=1
pkg-new/kairos --version

# 2. 比對設定範本與 unit 是否有變動，必要時更新 kairos.toml
diff /opt/kairos/kairos.example.toml pkg-new/kairos.example.toml
diff /etc/systemd/system/kairos.service pkg-new/kairos.service

# 3. 保留舊版以便回滾，換上新版
cp -p /opt/kairos/kairos /opt/kairos/kairos.prev
install -o root -g root -m 755 pkg-new/kairos /opt/kairos/kairos.new
mv /opt/kairos/kairos.new /opt/kairos/kairos
install -o root -g root -m 644 pkg-new/kairos.example.toml pkg-new/INSTALL.md \
  pkg-new/README.md pkg-new/PROTOCOL.md pkg-new/CHANGELOG.md pkg-new/THIRD-PARTY-LICENSES /opt/kairos/
# unit 有變動時：
# install -o root -g root -m 644 pkg-new/kairos.service /etc/systemd/system/kairos.service
# systemctl daemon-reload

# 4. 重新啟動並確認（第 9.1 節）
systemctl restart kairos
/opt/kairos/kairos --version
journalctl -u kairos -n 30 --no-pager
```

- 重新啟動時連線中的 Pulse 收到 `close_notify`，依退避在數秒內自動重連；不需要動 Pulse。
- 狀態表只在記憶體中：重啟後 `agents.json` 從空開始，離線中的 agent 會消失，線上的 agent 重連後再出現。
- 回滾：`mv /opt/kairos/kairos.prev /opt/kairos/kairos && systemctl restart kairos`。

## 12. 移除

```bash
systemctl disable --now kairos
rm /etc/systemd/system/kairos.service
systemctl daemon-reload
systemctl reset-failed kairos 2>/dev/null

# 需要保留資料時先備份
tar czf ~/kairos-data-$(date -u +%F).tar.gz -C /opt/kairos data logs kairos.toml

rm -rf /opt/kairos          # 含伺服器私鑰與所有 report 資料
userdel kairos
```

管理主機上的 `~/kairos-pki/`（CA 私鑰與簽發資料庫）不會被刪除，依組織政策另行處理。

## 13. 疑難排解

| 現象 | 原因與處理 |
|---|---|
| `status=226/NAMESPACE` | `ReadWritePaths` 指定的 `/opt/kairos/data` 或 `/opt/kairos/logs` 不存在。依第 4 節建立 |
| `status=200/CHDIR` | `/opt/kairos` 不存在或 kairos 使用者沒有執行權限（`ls -ld /opt/kairos` 應為 `root kairos drwxr-x---`） |
| `找不到設定檔 /opt/kairos/kairos.toml` | 尚未依第 7 節建立 `kairos.toml` |
| `未啟用客戶端憑證驗證（mTLS）卻監聽非 loopback 位址，拒絕啟動` | `kairos.toml` 沒有設定 `auth.client_ca`；依第 6、7 節啟用 mTLS |
| `無法開啟 key.pem: Permission denied` | 私鑰權限或群組不對：應為 `root:kairos` 640 |
| `無法監聽 0.0.0.0:443: Permission denied` | 監聽 1024 以下的連接埠需要 `AmbientCapabilities=CAP_NET_BIND_SERVICE`（第 8 節 drop-in） |
| 日誌在 journal 與 `logs/` 各一份 | 預設行為，見第 9.2 節 |
| 所有代理突然被拒，日誌出現 `ExpiredRevocationList` 或 `CRL … 已過期` | CRL 過期。依第 6.4 節重新產生並依第 10.1 節部署；之後設定第 10.3 節的 `kairos-crl.timer` |
| `[WARN] CRL … 將在 … 後過期` | CRL 即將過期：依第 6.4、10.1 節更新；使用 `kairos-crl.timer` 時檢查 `journalctl -u kairos-crl` |
| `[WARN] 伺服器憑證 … 將在 … 後到期` | 依第 6.1 節重新簽發，依第 10.1 節覆寫 |
| 其他錯誤訊息 | 見 `README.md`「疑難排解」 |
