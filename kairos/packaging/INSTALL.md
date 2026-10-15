# Kairos 安裝指南

本文件給地端管理者使用：把 Kairos 安裝成**系統層級 systemd 服務**。功能、設定欄位與協定細節請見同目錄的 `README.md` 與 `PROTOCOL.md`。

- **第 A 節（建議，0.8.0〔草稿〕起）**：以 deb／rpm 套件安裝，`kairos pki init` 一個指令建立 PKI。從下載到第一台 Pulse 連上約 10 個指令。
- **第 0～13 節**：從 tar.gz 發佈包手動安裝（不使用套件管理員的環境），以及共通的維運、升級與疑難排解。第 6 節同樣改用 `kairos pki init`；0.7 以前以 openssl 手動管理 PKI 的方式移到附錄 P。

> 不要用 `systemd-run --user` 或使用者層級的 unit 跑正式服務：沒有開啟 linger 時，最後一個登入 session 結束，user manager 會連同 Kairos 一起被停掉。

## A. 以 deb／rpm 套件安裝（0.8.0〔草稿〕）

支援：Debian 12、Ubuntu 22.04／24.04（deb），RHEL／Rocky／Alma 9（rpm）；x86_64 與 aarch64。套件的版面與 tar.gz 相同（`/opt/kairos`），另外安裝 `/usr/lib/systemd/system/kairos.service` 與 `/usr/bin/kairos`。

```bash
# A.1 下載與驗證（allowed_signers 必須從另一個管道取得，見第 2 節）
ssh-keygen -Y verify -f allowed_signers -I release@kairos-pulse -n kairos-pulse-release \
  -s SHA256SUMS.sig < SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS          # 下載的套件必須顯示 OK

# A.2 安裝（建立系統帳號 kairos、/opt/kairos/data、logs、root-ca，啟用但不啟動服務）
apt install ./kairos-<版本>-<commit>-linux-x86_64.deb          # Debian／Ubuntu
dnf install ./kairos-<版本>-<commit>-linux-x86_64.rpm          # RHEL／Rocky／Alma

# A.3 PKI：根 CA、伺服器憑證、代理簽發 CA 一次建立。--name 填 Pulse 連線時使用的所有名稱與 IP
sudo -u kairos kairos pki init --config /opt/kairos/kairos.toml \
  --name monitor.example.com --name 10.0.0.5 --root-dir /opt/kairos/root-ca

# A.4 根 CA 私鑰移到離線保存（加密的隨身碟等，保存兩份），再從伺服器刪除
tar -C /opt/kairos -cf - root-ca | gpg -c > /media/offline/kairos-root-ca-$(date -u +%F).tar.gpg
shred -u /opt/kairos/root-ca/root-ca.key && rm -rf /opt/kairos/root-ca/*

# A.5 啟動、建立首位管理員、建立註冊 token
systemctl start kairos
sudo -u kairos kairos users create admin --role admin --config /opt/kairos/kairos.toml   # stdout 為初始密碼
sudo -u kairos kairos enroll-token create --name rollout --group default --max-uses 50 \
  --config /opt/kairos/kairos.toml          # stdout 為 token；stderr 為安裝字串（pulse-enroll:v1:…）

# A.6 防火牆：Pulse 所在網段 → 8080（回報）、8444（註冊）；主控台 9443 只給管理網段（第 9.3 節）
ufw allow proto tcp from 10.0.0.0/16 to any port 8080,8444
```

- **預設設定**（`/opt/kairos/kairos.toml`，升級不覆寫）：監聽 `0.0.0.0:8080`、mTLS（`auth.client_ca` 指向 Kairos 管理的簽發 CA）、線上簽發、註冊端點 `open`（8444）、主控台 `127.0.0.1:9443`；所有路徑都是絕對路徑，`sudo -u kairos kairos …` 在任何目錄執行都使用同一份資料。`kairos pki init` 寫出的檔案正好是設定指向的位置，不需要編輯設定檔。完整欄位見 `/opt/kairos/kairos.example.toml`。
- `kairos pki init` 印出的 **CA 指紋**（`sha256:…`）就是安裝字串中的指紋；Pulse 以安裝字串註冊（Pulse 的 INSTALL 第 A 節，可用一行安裝腳本），不必再逐台簽發憑證。
- 根 CA 私鑰只在建立伺服器憑證與新的簽發 CA 時需要：到期或輪替時在離線主機以 `kairos pki sign` 簽發（第 10.4 節）。`/opt/kairos/root-ca` 對 Kairos 服務本身不可見（unit 的 `InaccessiblePaths=`）。
- 確認：`systemctl status kairos`、`journalctl -u kairos -n 30`（有「註冊端點（8444）已啟動」）、`sudo -u kairos kairos --check-config --config /opt/kairos/kairos.toml`。之後的日常維運、備份與升級見第 9～11 節（套件安裝時「換上新版」改為 `apt install ./新版.deb`／`dnf install ./新版.rpm`，見第 11.1 節）。
- **套件做的事**（`packaging/pkg/*.sh`）：安裝前建立系統帳號 `kairos`；安裝後建立 `data/`、`logs/`（`kairos:kairos` 750）與 `root-ca/`（700，只在新安裝），`systemctl enable kairos`；升級時 `systemctl try-restart kairos`；移除時 `systemctl disable --now kairos`。**移除與 purge 都不刪除 `/opt/kairos/data`**（資料庫與 `data/pki` 的私鑰），purge 只刪除日誌；系統帳號保留。
- 套件檔案與權限：`/opt/kairos` `root:kairos` 750；`kairos`（執行檔）755；`kairos.toml` `root:kairos` 640（設定檔，`dpkg` conffile／rpm `%config(noreplace)`）；文件與範例 644。以 `dpkg -L kairos`、`rpm -ql kairos` 查看清單。

## 0. 發佈包內容

每個版本的發佈檔（0.8.0〔草稿〕起；`<arch>` 為 `x86_64` 或 `aarch64`）：

| 檔案 | 說明 |
|---|---|
| `kairos-<版本>-<commit>-linux-<arch>.tar.gz` | 發佈包（本節以下的內容；第 1～13 節的手動安裝） |
| `kairos-<版本>-<commit>-linux-<arch>.deb`、`.rpm` | 套件（第 A 節）；內容與 tar.gz 相同，另有 systemd unit、預設設定與安裝腳本 |
| `kairos-<版本>-<commit>-linux-<arch>.cdx.json` | SBOM（與發佈包內的相同） |
| `SHA256SUMS`、`SHA256SUMS.sig` | 與 Pulse 共用的雜湊清單與簽章（第 2 節） |

發佈包 `kairos-<版本>-<commit>-linux-<arch>.tar.gz` 內含一層 `kairos-<版本>-<commit>-linux-<arch>/` 目錄：

| 檔案 | 權限 | 說明 |
|---|---|---|
| `kairos` | 755 | 執行檔（x86_64 或 aarch64、musl 靜態連結，不依賴系統函式庫） |
| `kairos.example.toml` | 644 | 設定範本，複製成 `kairos.toml` 後修改 |
| `kairos.service` | 644 | systemd unit 範本 |
| `kairos-crl.service`、`kairos-crl.timer` | 644 | 手動 PKI（附錄 P）的 CRL 定期重新產生與部署的 systemd 範例（第 10.3 節） |
| `prometheus.example.yml`、`kairos-alerts.example.yml` | 644 | Prometheus 抓取設定與告警規則範例（第 9.2a 節，0.6.0） |
| `INSTALL.md` | 644 | 本文件 |
| `README.md`、`PROTOCOL.md` | 644 | 說明文件與協定規格 |
| `STORAGE.md` | 644 | 資料儲存（SQLite）：schema、保留與容量上限、容量估算、備份與還原 |
| `API.md` | 644 | 主控台 API（HTTPS，`/api/v1`）：端點、驗證（API token）、即時推送、量測 |
| `PKI.md` | 644 | 註冊通道的 CA 架構、線上／離線簽發、簽發 CA 的輪替、CRL、資料模型與量測（0.5.0）；`kairos pki init／sign／gencrl`（第 15 節，0.8.0） |
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
| 主控台 API | `127.0.0.1:9443`（預設只綁 loopback；開放給管理網段見第 9.3 節） |
| 伺服器名稱 | `monitor.example.com`、`10.0.0.5`（換成實際值） |
| 根 CA 的暫存目錄 | `/opt/kairos/root-ca`（`kairos pki init` 寫出根 CA 私鑰；**之後移到離線保存**） |

指令以 root 執行（或在前面加 `sudo`）。

## 1. 需求

- Linux x86_64 或 aarch64（0.8.0〔草稿〕起）、systemd（建議 247 以上；Ubuntu 22.04／24.04、Debian 12、RHEL 9 皆可）
- 不需要 OpenSSL：PKI 由 `kairos pki init` 建立（第 6 節）。只有沿用附錄 P 的手動 PKI 才需要管理主機上的 OpenSSL 3.0 以上
- 驗證發佈包簽章用的 OpenSSH 8.1 以上（`ssh -V`；可在管理主機上驗證，見第 2 節）
- 防火牆允許 Pulse 連到監聽埠（本例 TCP 8080）
- 啟用註冊通道（0.5.0）時，防火牆允許**要註冊的主機**連到 TCP 8444（第 7 節的 `[enroll]`）；不使用自動註冊時不必開放（沒有 `[enroll]` 時 Kairos 不監聽 8444）
- 主控台 API（TCP 9443）**只對管理網段開放**：預設只綁 `127.0.0.1`；改為對外位址時以防火牆限制來源（第 9.3 節）。不要對 agent 所在的網段或網際網路開放

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
file pkg/kairos              # 應為 ELF 64-bit x86-64（或 ARM aarch64）static-pie linked
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

`data/` 會放 SQLite 資料庫 `kairos.db`（report、彙總與主機清冊），**必須在本機磁碟**（SQLite 的 WAL 不支援 NFS、SMB 等網路磁碟），並預留 `storage.max_total_bytes`（預設 64 GiB）加上至少 128 MiB 的空間；主機數與保留天數對應的容量見 `STORAGE.md`「容量估算」。

## 5. 安裝執行檔與文件

```bash
cd ~/kairos-dist/pkg
install -o root -g root -m 755 kairos /opt/kairos/kairos
install -o root -g root -m 644 kairos.example.toml INSTALL.md README.md PROTOCOL.md \
  STORAGE.md API.md PKI.md CHANGELOG.md THIRD-PARTY-LICENSES /opt/kairos/
```

## 6. 建立 PKI（kairos pki init，0.8.0〔草稿〕）

`kairos pki init` 一次建立 Kairos 需要的三樣東西（`PKI.md` 第 15 節），取代 0.7 以前以 openssl 手寫 `ca.cnf` 的流程（舊流程見附錄 P，既有部署照常可用）：

| 產出 | 內容 | 位置 |
|---|---|---|
| 根 CA | ECDSA P-384、10 年；**Pulse 信任的對象**（安裝字串的 CA 指紋） | 私鑰 `<--root-dir>/root-ca.key`（0600，**之後移到離線保存**）；憑證另放 `data/pki/root-ca.pem`（`pki.server_ca_path`） |
| 伺服器憑證 | ECDSA P-256、825 天；SAN＝所有 `--name`、EKU `serverAuth`、`CA:FALSE`（8443、8444、主控台共用） | `data/pki/server.pem`、`data/pki/server.key`（0600） |
| 代理簽發 CA | ECDSA P-256、5 年；由根 CA 簽發、已安裝並啟用（線上簽發 Pulse 的用戶端憑證） | `data/pki/issuing-1.key`（0600）、資料庫；信任錨 `data/pki/issuing-cas.pem`（`auth.client_ca`） |

```bash
# 設定檔（pki init 讀取 storage.data_dir 等；第 7 節再填入 init 印出的值）
install -o root -g kairos -m 640 /opt/kairos/kairos.example.toml /opt/kairos/kairos.toml
# 根 CA 私鑰的暫存目錄（Kairos 服務本身看不到，unit 的 InaccessiblePaths=）
install -d -o kairos -g kairos -m 700 /opt/kairos/root-ca
cd /opt/kairos     # kairos.toml 的相對路徑（data/ 等）以目前目錄解析
sudo -u kairos /opt/kairos/kairos pki init --config /opt/kairos/kairos.toml \
  --name monitor.example.com --name 10.0.0.5 --root-dir /opt/kairos/root-ca
```

- `--name`：Pulse 連線時使用的**所有**名稱與 IP（可重複；DNS 名稱或 IP，不接受萬用字元）。之後要增加名稱時以 `kairos pki sign --profile server` 重新簽發（第 10.4 節）。
- 只能執行一次：資料庫已有簽發 CA、或任何一個要寫出的檔案已存在時拒絕（結束代碼 11），不會覆寫。
- 組織名稱（憑證的 CN）預設取 `pki.organization`（預設 `Kairos`），可用 `--organization` 指定。
- 完成時印出要放進 `kairos.toml` 的設定（第 7 節）與 **CA 指紋**，並以 `KRS-W1306`〔暫定〕提醒：**立即把 `/opt/kairos/root-ca` 複製到離線媒體（加密、兩份）後從伺服器刪除**（`shred -u /opt/kairos/root-ca/root-ca.key`）。根 CA 私鑰外洩時，任何人都能簽出 Pulse 會信任的伺服器憑證。
- 不想讓任何 CA 私鑰放在伺服器上時（離線簽發模式，`PKI.md` 第 6 節）：不使用 `pki init`，沿用附錄 P 的手動 PKI（`kairos pki sign`、`gencrl` 可取代其中的 openssl 指令）。

`kairos pki` 的完整說明：`kairos pki --help`、`PKI.md` 第 15 節（`init`、`sign`、`gencrl`、`revocations export`）。

## 7. 設定

編輯 `/opt/kairos/kairos.toml`（完整欄位見 `README.md`「設定」），填入第 6 節 `kairos pki init` 印出的值：

```toml
[server]
listen = "0.0.0.0:8080"                    # 預設只聽 127.0.0.1，其他主機連不進來
cert_path = "data/pki/server.pem"
key_path = "data/pki/server.key"
# max_connections = 1024                   # 調高時，unit 的 LimitNOFILE 也要 > max_connections + 64

[auth]
client_ca = "data/pki/issuing-cas.pem"     # 啟用 mTLS：只信任 Kairos 管理的簽發 CA（PKI.md 第 15.1 節）

[enroll]
mode = "open"                              # 註冊通道（8444）：受理 pulse enroll

[pki]
mode = "online"
server_ca_path = "data/pki/root-ca.pem"    # Pulse 信任的根 CA；安裝字串的 CA 指紋
```

`server.listen` 不是 loopback 位址（`127.0.0.0/8`、`::1`）又沒有設定 `auth.client_ca` 時，Kairos 會拒絕啟動並記錄 `ERROR`「未啟用客戶端憑證驗證（mTLS）卻監聽非 loopback 位址，拒絕啟動」。`auth.allow_unauthenticated = true` 可以略過這項檢查，但任何能連線的人都能以任意 `agent_id` 註冊，**不可用於正式環境**。

相對路徑以工作目錄 `/opt/kairos` 解析（unit 的 `WorkingDirectory=`；CLI 請先 `cd /opt/kairos`，或像套件的預設設定一樣改寫成絕對路徑）。沿用附錄 P 手動 PKI 的部署，`cert_path`、`key_path`、`client_ca`、`crl` 指向附錄 P.4 放置的檔案。

檔案權限總覽：

| 路徑 | 擁有者 | 權限 |
|---|---|---|
| `/opt/kairos` | `root:kairos` | 750 |
| `kairos`（執行檔） | `root:root` | 755 |
| `kairos.toml`、`key.pem` | `root:kairos` | 640（`key.pem` 由 root 擁有、只讓 `kairos` 群組讀取：服務帳號讀得到但不能改寫或替換） |
| `cert.pem`、`client-ca.pem`、`agents.crl` | `root:kairos` | 644 |
| `data/`、`logs/` | `kairos:kairos` | 750 |
| `data/pki/`（0.5.0；`kairos pki init`、Kairos 建立；`server.key`、`issuing-*.key` 600） | `kairos:kairos` | 700；簽發 CA 私鑰 `issuing-*.key` 600（群組或其他使用者有任何權限時不使用，`KRS-E1303`） |
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
ss -ltnp | grep kairos                       # 監聽 0.0.0.0:8080 與主控台 127.0.0.1:9443
tail -n 20 /opt/kairos/logs/kairos-$(date -u +%F).log
cat /opt/kairos/data/agents.json             # 每 5 秒更新；online 等於已連線的 Pulse 數
# 主機清冊（SQLite）：Pulse 上線後 30 秒內出現；--offline-for 3d 列出失聯 3 天以上的主機
sudo -u kairos /opt/kairos/kairos hosts list --config /opt/kairos/kairos.toml
```

啟動成功時日誌會有：

```
[INFO] kairos <版本> (<commit>)
[INFO] 設定檔: /opt/kairos/kairos.toml
[INFO] 已啟用客戶端憑證驗證（mTLS），信任 data/pki/issuing-cas.pem；agent_id 必須與憑證的 SAN 相符
[INFO] 已建立資料庫 schema v1                  （第一次啟動時）
[INFO] SQLite 資料庫 data/kairos.db（schema v1）：主機清冊 0 台、指標序列 0 個
[INFO] 主控台 API 已啟動，監聽 https://127.0.0.1:9443/api/v1/（連線上限 64，SSE 上限 16，憑證 cert.pem）
[INFO] Kairos 已啟動，監聽 0.0.0.0:8080（連線上限 1024，…）
```

- `systemctl status kairos` 顯示 `(code=exited, status=3)` 且服務停在 `failed`、沒有重試：設定錯誤，用 `journalctl -u kairos -n 30` 看錯誤訊息，修正 `kairos.toml` 後 `systemctl restart kairos`。
- 顯示 `status=4` 且停在 `failed`：憑證錯誤（伺服器憑證、私鑰、`client-ca.pem`、CRL 讀不到、格式錯誤或憑證與私鑰不成對；常見原因是檔案權限或路徑），錯誤訊息在 `journalctl -u kairos -n 30` 與 `logs/`。修正後 `systemctl restart kairos`。`status=2` 為 `ExecStart` 的命令列參數錯誤。
- 顯示 `status=11` 並每 5 秒重試：`server.listen` 或主控台 `console.listen` 的連接埠被其他程式佔用（`ss -ltnp | grep -E ':8080|:9443'` 查看是誰；日誌為「無法監聽…」或「無法監聽主控台 console.listen …」）。不需要主控台時可在 `kairos.toml` 加上 `[console]` 的 `enabled = false`。
- 顯示 `status=3` 且日誌有 `KRS-E0202`：`server.listen` 與 `console.listen` 使用同一個位址與連接埠，改其中一個。
- 啟動失敗並出現 `未啟用客戶端憑證驗證（mTLS）卻監聽非 loopback 位址，拒絕啟動`：`kairos.toml` 沒有設定 `auth.client_ca`。依第 6、7 節啟用 mTLS。
- 出現 `未啟用客戶端憑證驗證（auth.client_ca）卻監聽非 loopback 位址 …（auth.allow_unauthenticated = true）` 的 `WARN`：設定了 `auth.allow_unauthenticated = true`，正式環境不可如此。
- 出現 `CRL … 將在 … 後過期` 或 `伺服器憑證 … 將在 … 後到期` 的 `WARN`（已過期時為 `ERROR`）：依第 10.1、10.3 節更新。
- 出現 `伺服器私鑰 … 的權限過寬` 的 `WARN`：`key.pem` 讓其他使用者有權限、group 可寫入，或 group 可讀但群組不是 `kairos`。依訊息執行 `chown root:kairos /opt/kairos/key.pem && chmod 640 /opt/kairos/key.pem`。Kairos 在啟動時與熱重載換了私鑰時檢查，只記錄不拒絕啟動。
- 出現 `可開啟的檔案數上限（RLIMIT_NOFILE soft limit）…` 的 `WARN`：unit 的 `LimitNOFILE` 沒有生效或小於 `server.max_connections` ＋ `console.max_connections`（預設 64）＋ 64；依訊息中的建議值以 drop-in 調高 `LimitNOFILE`（`systemctl show kairos -p LimitNOFILE` 可確認目前值）。
- 從其他主機確認 TLS 與伺服器憑證：`openssl s_client -connect monitor.example.com:8080 -CAfile root-ca.pem -servername monitor.example.com </dev/null`（`root-ca.pem` 為 `data/pki/root-ca.pem` 的複本）（啟用 mTLS 時，沒帶客戶端憑證會在握手後被拒，這是正常的）。
- Pulse 上線後，`agents.json` 的 `online` 會增加。多台 Pulse 經同一個 NAT 連入時，**以 `agents.json` 作為上線狀態的依據**：日誌的上線／離線事件有節流（見 `README.md`「日誌」）。

### 9.2 日誌放在哪裡

Kairos 的日誌同時寫到 stdout 與 `/opt/kairos/logs/kairos-YYYY-MM-DD.log`（UTC 日期分檔，依 `log.retention_days` 清除）。在預設的 unit 下 stdout 進 journal，所以同一份日誌會存兩份：

- 保留預設：可用 `journalctl -u kairos` 查詢，與 systemd 的啟停紀錄放在一起。
- 只要檔案一份：以 drop-in 設定 `StandardOutput=null`，或在 `kairos.toml` 設定 `log.stdout = false`（0.6〔草稿〕）。啟動失敗等日誌初始化前的錯誤寫在 stderr，仍會進 journal。

交給 SIEM（0.6〔草稿〕，審計 O3、S7）：`log.format = "json"`（檔名改為 `kairos-YYYY-MM-DD.jsonl`，一行一個 JSON、UTC、固定欄位 `ts`、`level`、`event`、`code`、`msg`、`fields`…），由 Filebeat／Fluent Bit 收集；或設定 `[log.syslog]` 的 `target`（`udp://`、`tcp://`、`unix:///dev/log`）以 RFC 5424 直接送出。`log.format = "legacy"` 保留 0.5 以前的行格式供既有的解析規則過渡（`text` 的行首改為 RFC 3339 帶時區，例如 `2026-10-14T14:22:16.123+08:00 [INFO] …`）。

### 9.2a 健康檢查、Prometheus 與診斷包（0.6〔草稿〕）

```bash
# 設定檢查（不啟動服務、不連線）：0 通過、8 有警告（--strict）、3 設定錯誤、4 憑證錯誤
sudo -u kairos /opt/kairos/kairos --check-config --config /opt/kairos/kairos.toml

# 健康檢查（主控台的監聽上；不需要 token，只回傳狀態字）
curl -sk https://127.0.0.1:9443/healthz        # {"status":"ok"}
curl -sk https://127.0.0.1:9443/readyz         # 200 就緒／503 不就緒，本文為各項的狀態
curl -sk https://127.0.0.1:9443/metrics | head # Prometheus 文字格式；未設定 token 時只接受 loopback

# 支援診斷包（tar.gz，0600；遮蔽 token、密碼、私鑰）。交給外部廠商時加 --redact-hosts
cd /tmp && sudo /opt/kairos/kairos diag --config /opt/kairos/kairos.toml
```

- **負載平衡器的探針**：以 `GET /readyz` 判斷（200／503）。主控台不對外開放時，在 `kairos.toml` 設定 `[observability] listen = "<負載平衡器可達的位址>:9464"`（純 HTTP，只有 `/healthz`、`/readyz`、`/metrics` 三個端點，`/metrics` 仍只接受 loopback 或帶 token 的請求）；只負責 8444 的負載平衡器用 `/readyz?scope=enroll`。
- **Prometheus**：同一台主機時抓 `http://127.0.0.1:9464/metrics`（`observability.listen = "127.0.0.1:9464"`）；從其他主機抓取時設定 `observability.metrics_token_file`（`openssl rand -hex 32 > /opt/kairos/metrics.token && chown root:kairos /opt/kairos/metrics.token && chmod 640 /opt/kairos/metrics.token`），經主控台的 HTTPS 以 bearer token 抓取。設定與建議的告警規則見 `prometheus.example.yml`、`kairos-alerts.example.yml`。
- **經反向代理（nginx、Apache、HAProxy 等）提供 `/metrics` 時，必須設定 `observability.metrics_token_file`**，並讓 Prometheus 帶 bearer token。代理與 Kairos 在同一台主機時，轉送進來的請求來源都是 `127.0.0.1`，「未設定 token 時只接受 loopback」會變成任何能連到代理的人都能讀取。0.7〔草稿〕起，未設定 token 時帶有 `Forwarded`、`X-Forwarded-For`、`X-Forwarded-Host`、`X-Forwarded-Proto`、`X-Real-IP` 或 `Via` 標頭的 `/metrics` 請求一律回 403，並在第一次時記一行 `WARN`（`observability.metrics_proxied`，`KRS-W0304`）；但**不加這些標頭的代理（例如 nginx 只寫 `proxy_pass` 的預設設定）Kairos 無法發現**，所以不能以這個檢查代替 token。
- 另外建議在 Kairos 以外監看 `/healthz`（例如既有的 Zabbix）：Kairos 自己無法通知「自己已經停止」。
- **稽核鏈尾的外送**（0.7〔草稿〕）：Kairos 每 `audit.anchor_log_minutes` 分鐘（預設 60）寫一行 `audit.chain_anchor`（INFO，含最後一筆稽核紀錄的 id 與雜湊）。設定 `[log.syslog]` 或以日誌收集器把這些行送到**另一台主機**保存，才有資料庫之外的副本（`API.md` 第 12.8 節）。

### 9.3 主控台 API

主控台 API（`API.md`）以 HTTPS 監聽 `console.listen`，預設 `127.0.0.1:9443`，憑證沿用 `cert.pem`／`key.pem`。所有端點都需要驗證：整合與腳本用 API token，人員以帳號登入（工作階段，0.6.0；第 9.4 節）。

```bash
# 建立唯讀 token（角色 viewer；stdout 只有 token，只顯示這一次；預設 90 天後到期）。
# 寫入操作需要對應的角色：--role operator（核准、拒絕、撤銷）或 --role admin（模式、政策、使用者）；
# 或以 --user <帳號> 讓 token 擁有該使用者的權限（API.md 第 12.1 節）
TOKEN=$(sudo -u kairos /opt/kairos/kairos api-token create --name ops-check --config /opt/kairos/kairos.toml)
# 在 Kairos 主機上確認（-k：自簽或私有 CA 的憑證；正式使用請以 --cacert 指定 CA）
curl -sk -H "Authorization: Bearer $TOKEN" https://127.0.0.1:9443/api/v1/system/health
curl -sk -H "Authorization: Bearer $TOKEN" "https://127.0.0.1:9443/api/v1/hosts?status=offline"
# 列出、撤銷
sudo -u kairos /opt/kairos/kairos api-token list --config /opt/kairos/kairos.toml
sudo -u kairos /opt/kairos/kairos api-token revoke <token_id> --reason "確認完畢" --yes --config /opt/kairos/kairos.toml
```

- 日誌會出現「API token 已建立：token_id …」（10 秒內；不含 token 本身）。
- **開放給管理網段**：把 `kairos.toml` 的 `[console]` 改為 `listen = "0.0.0.0:9443"` 後重新啟動，並以防火牆**只允許管理網段**連到 9443，例如：

  ```bash
  # ufw（Ubuntu）
  ufw allow from 10.0.100.0/24 to any port 9443 proto tcp
  # firewalld（RHEL）
  firewall-cmd --permanent --add-rich-rule='rule family="ipv4" source address="10.0.100.0/24" port port="9443" protocol="tcp" accept'
  firewall-cmd --reload
  ```

  agent 連線埠（8080／8443）與主控台分開：agent 所在的網段不需要、也不應該能連到 9443。
- 瀏覽器或整合要信任主控台的憑證時，建議另外簽發 SAN 為主控台網址的憑證，設定 `console.cert_path`／`console.key_path`（同樣熱重載）。
- 主控台需要 SQLite（`storage.backend = "sqlite"`，預設）；`backend = "jsonl"` 時不啟動並記 `WARN`。

### 9.4 建立首位管理員（0.6.0）

Kairos **沒有預設的帳號與密碼**。安裝（或從 0.5 升級）後，以服務帳號建立第一位管理員；Kairos 執行中也可以：

```bash
sudo -u kairos /opt/kairos/kairos users create admin --role admin --display-name "系統管理員" \
  --config /opt/kairos/kairos.toml
```

- stdout 的那一行是**一次性的初始密碼**（只顯示這一次，4 組 5 個字元）：立即交給管理員本人，第一次登入主控台時必須變更（變更前其他功能都回 `password_change_required`）。
- 要自行指定密碼時加上 `--password-stdin`，從檔案或管線提供（標準輸入是終端機時會被拒絕，避免密碼顯示在畫面上）：

  ```bash
  install -m 600 /dev/null /root/admin.pw && vi /root/admin.pw      # 寫入至少 12 個字元的密碼
  sudo -u kairos /opt/kairos/kairos users create admin --role admin --password-stdin \
    --config /opt/kairos/kairos.toml < /root/admin.pw && shred -u /root/admin.pw
  ```

- 登入主控台（0.7.0〔草稿〕起有網頁介面，見 9.5）：以瀏覽器開啟 `https://<Kairos 主機>:9443/`，以 `admin` 與初始密碼登入，畫面會要求先變更密碼。沒有瀏覽器時也可以 curl 確認：

  ```bash
  curl -sk -c cookies.txt -H 'Content-Type: application/json'     -d '{"username":"admin","password":"<初始密碼>"}' https://127.0.0.1:9443/api/v1/auth/login
  ```

  回應的 `csrf_token` 要放在之後每個寫入請求的 `X-CSRF-Token` 標頭（例如 `POST /api/v1/me/password` 變更密碼）。
- 建議接著：在主控台的「個人設定」啟用兩步驟驗證（或 `POST /api/v1/me/totp/setup`、`/enable`；10 組復原碼離線保存）；為其他人建立各自的帳號（`kairos users create <帳號> --role operator|auditor|viewer`），不要共用 `admin`；至少保留兩位管理員（最後一位管理員不能被停用或移除）。
- 確認稽核日誌：`sudo -u kairos /opt/kairos/kairos audit verify --config /opt/kairos/kairos.toml`（完整時結束代碼 0；斷裂時 9、`KRS-E6201`）。
- 使用 AD／LDAP 時，在 `kairos.toml` 設定 `[ldap]`（`ca_file` 必填；服務帳號密碼放在 `chmod 600` 的 `bind_password_file`），重新啟動後以 `kairos users map-group "<群組 DN>" --role operator` 對應角色（API.md 第 12.7 節）。
- 主控台金鑰 `data/console.key`（0600）在第一次啟動主控台時建立，用於 CSRF 與 TOTP 秘密的加密；備份資料庫時一併（加密）備份，遺失時已綁定的 TOTP 需以 `kairos users reset-mfa` 重設。

### 9.5 開啟主控台（0.7.0〔草稿〕）

網頁介面與 API 在同一個 HTTPS 監聽上（`console.listen`，預設 `127.0.0.1:9443`），網頁檔已嵌入 `kairos` 執行檔，不需要另外安裝。

1. **連線方式**：預設只綁 loopback。
   - 在 Kairos 主機上：開啟 `https://127.0.0.1:9443/`。
   - 從管理電腦經 SSH 轉送：`ssh -L 9443:127.0.0.1:9443 <管理者>@<Kairos 主機>`，再開啟 `https://127.0.0.1:9443/`。
   - 開放給管理網段：在 `kairos.toml` 的 `[console]` 設定 `listen = "0.0.0.0:9443"`，重新啟動 Kairos，並以防火牆只允許管理網段連到 9443。
2. **憑證**：預設沿用 agent 連接埠的伺服器憑證（`server.cert_path`），瀏覽器通常會顯示「不受信任」。正式使用時以 `console.cert_path`／`console.key_path` 設定由組織 CA 簽發、SAN 為主控台網址（例如 `monitor.example.gov.tw`）的憑證（依 `server.reload_interval_secs` 熱重載），並把組織 CA 匯入管理電腦。
3. **首位管理員登入**：以 9.4 建立的 `admin` 與一次性初始密碼登入，畫面會先要求變更密碼（至少 12 個字元、不包含帳號、不是常見密碼）；完成後進入「總覽」。接著在右上角「個人設定」啟用兩步驟驗證並保存 10 組復原碼。
4. **確認**：總覽的六個狀態磚與「資料時間」、主機清單的總數與 `kairos hosts list` 一致；右上角的系統狀態燈顯示系統健康的整體狀態與第一個需要注意的項目（細節以 `GET /api/v1/system/health` 或 `kairos --check-config` 查看；系統健康畫面之後提供）。
5. **瀏覽器需求**：0.7 支援 **Chromium 系（Chrome、Edge）** 最近的版本（Trusted Types 需要 Chromium）；Firefox、Safari 可以使用，但未在發佈前驗證。需要啟用 JavaScript；不需要外部網路（所有檔案都由 Kairos 提供）。

- 閒置 30 分鐘（`accounts.session_idle_minutes`）沒有操作時自動登出，即使總覽仍在自動更新。
- 0.7.0 的畫面：登入、總覽、主機清單、主機詳情、個人設定。告警、註冊與憑證、使用者、稽核、系統設定請先以 CLI（`kairos alerts`、`kairos enroll`、`kairos users`、`kairos audit`…）或 API 操作。
- 反向代理：可以放在 nginx 等之後，代理必須保留原本的 `Host`（CSRF 檢查以 `Origin` 與 `Host` 比對）、不要緩衝 `/api/v1/events`（SSE）；所有使用者會共用代理的來源位址與登入失敗額度（API.md 第 4.3 節）。

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
- SQLite 資料庫：以線上備份產生一致的快照，**不要在執行中直接複製 `data/kairos.db`**（最新的交易可能還在 `kairos.db-wal` 中）。例如每天一次：

  ```bash
  install -d -o kairos -g kairos -m 750 /var/backups/kairos
  sudo -u kairos /opt/kairos/kairos db backup /var/backups/kairos/kairos-$(date -u +%F).db \
    --config /opt/kairos/kairos.toml
  ```

  備份檔不會覆寫既有的檔案；舊備份請自行輪替（例如 `find /var/backups/kairos -name 'kairos-*.db' -mtime +14 -delete`）。還原步驟見 `STORAGE.md`「備份與還原」。停機備份時直接複製整個 `data/` 即可。
- 使用 JSONL（`storage.backend = "jsonl"` 或鏡像）時另備份 `/opt/kairos/data/reports-*.jsonl`；搬移或備份時一併保留 `data/.retention-guard.json` 與 `logs/.retention-guard-logs.json`，不要刪除
- 離線保存的根 CA（第 6 節的 `root-ca/`；手動 PKI 為管理主機的 `~/kairos-pki/`）
- **資料目錄中的金鑰檔**（`kairos db backup` 的資料庫備份**不含**，資料庫中只有以它們加密的密文或 HMAC）：
  - `data/alerts-secrets.key`（0.6.0 起）：通知管道機密（SMTP 密碼、Webhook 網址、HMAC 金鑰、LINE token）的 AES-256-GCM 金鑰。
  - `data/console.key`（0.6.0 起）：主控台的 CSRF 與 TOTP 秘密的加密金鑰。
  - `data/audit-hmac.key`（0.7〔草稿〕起）：稽核雜湊鏈的 HMAC 金鑰（`API.md` 第 12.8 節）。

  **以 `--include-keys` 一併加密備份**（0.7〔草稿〕；預設不含，需要另外指定）：金鑰檔以備份密碼加密（Argon2id 衍生金鑰＋AES-256-GCM），寫成與資料庫備份並列的 `<備份檔>.keys`（0600）。密碼只能從檔案或 stdin 讀取（不接受命令列上的密碼，避免留在 shell 歷史與行程清單），至少 12 個字元；**請把備份密碼與備份分開保管**（例如密碼管理工具），密碼遺失時無法解開。

  ```bash
  # 備份密碼檔：只有 root 可讀（sudo -u kairos 執行時以 stdin 傳入，服務帳號不需要讀取它）
  install -m 600 /dev/null /root/kairos-backup.pass && openssl rand -base64 24 > /root/kairos-backup.pass
  sudo -u kairos /opt/kairos/kairos db backup /var/backups/kairos/kairos-$(date -u +%F).db \
    --include-keys --password-stdin --config /opt/kairos/kairos.toml < /root/kairos-backup.pass
  # → kairos-YYYY-MM-DD.db（資料庫，0640）與 kairos-YYYY-MM-DD.db.keys（加密的金鑰檔，0600）
  ```

  `.keys` 可以與資料庫備份分開存放（例如另一個保管位置），資料庫備份外流時金鑰不會一起外流；兩者都外流時，強度取決於備份密碼。不使用 `--include-keys` 時，請自行以加密方式保存這三個檔案（例如 `tar -C /opt/kairos/data -cf - alerts-secrets.key console.key audit-hmac.key | gpg -c > keys-$(date -u +%F).tar.gpg`）。

  **還原**（同一台或新主機；資料庫與金鑰要是同一次備份的組合，不要拿新金鑰配舊資料庫）：

  ```bash
  sudo systemctl stop kairos
  # 1. 資料庫：複製備份並刪除舊的 WAL（STORAGE.md「備份與還原」）
  sudo -u kairos cp /var/backups/kairos/kairos-2026-10-15.db /opt/kairos/data/kairos.db
  sudo -u kairos rm -f /opt/kairos/data/kairos.db-wal /opt/kairos/data/kairos.db-shm
  # 2. 金鑰檔：解開並寫回 data/（0600；以 root 執行時擁有者改為資料目錄的擁有者）
  sudo -u kairos /opt/kairos/kairos db restore-keys /var/backups/kairos/kairos-2026-10-15.db.keys \
    --password-stdin --config /opt/kairos/kairos.toml < /root/kairos-backup.pass
  sudo systemctl start kairos
  sudo -u kairos /opt/kairos/kairos audit verify --config /opt/kairos/kairos.toml
  ```

  - 密碼錯誤與 `.keys` 被修改、截斷無法區分，都以`KRS-E4307` 拒絕，資料目錄不會被寫入任何檔案。
  - `data/` 已有內容不同的金鑰檔時（例如 Kairos 在還原前已啟動並產生了新的金鑰），`restore-keys` 拒絕；確認後加 `--overwrite`，原本的檔案改名為 `<名稱>.replaced-<時間>` 保留。
  - 少了 `alerts-secrets.key`（或與資料庫不成對）時，通知管道的狀態為 `secrets_unavailable`（`KRS-E5110`，主控台、`kairos alert-channels list` 與「通知管道失敗」事件都會顯示），不會發送；還原金鑰檔後重新啟動即恢復，或重新輸入機密。少了 `console.key` 時 Kairos 會建立新的金鑰，已綁定的 TOTP 需以 `kairos users reset-mfa` 重設，主控台需重新取得 CSRF token（重新整理頁面）。少了 `audit-hmac.key` 時，`kairos audit verify` 與每日的自我驗證以 `hmac_key_unavailable`（`KRS-E6201`）失敗，並開「稽核日誌完整性」緊急事件。
- 使用線上簽發（第 6 節的 `pki init`）時另備份 `/opt/kairos/data/pki/`（**含簽發 CA 私鑰**，`kairos db backup` 不含；加密保存，例如 `tar czf - -C /opt/kairos/data pki | gpg -c > pki-$(date -u +%F).tgz.gpg`）。遺失時已簽發的憑證仍有效，但無法再撤銷，需立即建立新的簽發 CA（`PKI.md` 第 4 節）

### 10.3 定期更新 CRL（kairos-crl.timer；只用於附錄 P 的手動 PKI）

以 `kairos pki init` 建立的線上簽發 CA 不需要這一節：Kairos 每 24 小時與每次撤銷自己重新產生 CRL（`PKI.md` 第 10 節）。以下只適用於 `auth.crl` 由外部 CA 產生的部署（附錄 P、離線簽發模式）；其中的 `openssl ca -gencrl` 可以改用 `kairos pki gencrl --revocations`（`PKI.md` 第 15.3 節）。

發佈包附有 `kairos-crl.service`（重新產生 CRL、檢查後部署）與 `kairos-crl.timer`（每週執行一次）範例。安裝在**保存代理 CA 的主機**（第 P.2 節的 `agent-ca/`）上；`kairos-crl.service` 依序：

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

### 10.4 伺服器憑證到期、增加名稱（kairos pki sign，0.8.0〔草稿〕）

伺服器憑證到期前 30 天（`server.cert_expiry_warning_days`）起 Kairos 會記 `WARN`。以離線保存的根 CA 重新簽發（在離線主機執行；`kairos` 執行檔不需要設定與資料庫）：

```bash
# 離線主機：產生新的私鑰與憑證（--name 列出全部名稱；根 CA 私鑰必須是 0600）
kairos pki sign --profile server --ca-cert root-ca/root-ca.pem --ca-key root-ca/root-ca.key   --name monitor.example.com --name 10.0.0.5 --new-key server.key --out server.pem
# 帶回 Kairos 主機，原子覆寫（Kairos 在下一個重載週期載入，不需重新啟動；第 10.1 節）
install -o kairos -g kairos -m 600 server.key /opt/kairos/data/pki/server.key.new
install -o kairos -g kairos -m 644 server.pem /opt/kairos/data/pki/server.pem.new
mv /opt/kairos/data/pki/server.key.new /opt/kairos/data/pki/server.key
mv /opt/kairos/data/pki/server.pem.new /opt/kairos/data/pki/server.pem
```

- `pki sign` 在根 CA 私鑰所在的目錄寫一筆紀錄到 `kairos-ca.log`（0600），輸出檔已存在時不覆寫。私鑰不想離開 Kairos 主機時，改以 `--csr` 簽發 Kairos 主機產生的 CSR。
- 代理簽發 CA 的輪替（5 年；`KRS-W1304` 提醒）：`kairos pki issuing-ca new` → 在離線主機 `kairos pki sign --profile issuing-ca --csr issuing-N.csr …` → `issuing-ca install`、`activate`（`PKI.md` 第 4 節）。

## 11. 升級

**未啟用 mTLS 的部署請先處理**：從舊版升級時，若 `kairos.toml` 的 `server.listen` 是非 loopback 位址（例如 `0.0.0.0:8080`）而沒有設定 `auth.client_ca`，新版會拒絕啟動（舊版只記 `WARN`）。升級前先依第 6、7 節啟用 mTLS；只有隔離的測試環境才在 `[auth]` 加上 `allow_unauthenticated = true`。

**從 0.2 升級到 0.3：report 改寫 SQLite**。升級後 report 寫入 `data/kairos.db`，不再寫 `reports-*.jsonl`；直接讀 JSONL 的工具請先改用 `kairos reports export`，或在 `kairos.toml` 的 `[storage]` 加上 `jsonl_mirror = true`（同時寫兩份），或暫時設定 `backend = "jsonl"` 維持舊行為。既有的 JSONL 檔不會自動匯入資料庫，不開鏡像時也不會再被自動清除；需要舊資料的趨勢與匯出時，以 `kairos reports import` 匯入（下方步驟 3a，**需要停止 Kairos**，`STORAGE.md`「匯入舊的 JSONL」）。之後每次升級 Kairos 前先以 `kairos db backup` 備份：新版可能升級 schema，升級後的資料庫無法給舊版使用（舊版會拒絕啟動）。詳見 `STORAGE.md`「從 JSONL（0.2）升級」。

**從 0.2 升級到 0.3：先升級 Kairos，再升級 Pulse**。Pulse 0.3 預設啟用本機緩衝，與 Kairos 0.3 協商 `durable` 後，Kairos 停機或網路中斷期間的取樣會在恢復後補送（at-least-once），Kairos 以唯一索引去重。Kairos 先升級後，Pulse 升級後的第一次斷線就能補送；Pulse 0.2 連 Kairos 0.3 時與 0.2 相同（at-most-once）。Pulse 端要先建立 `buffer/` 目錄並換上新版 unit（Pulse 的 INSTALL 第 10 節）。補送的資料以 Pulse 的取樣時間保存與彙總，`kairos reports export` 的 `ts` 可能比 `recv_at` 早數小時到數天；縮短 `storage.raw_retention_days` 時請一併檢查 Pulse 的 `buffer.max_age_hours`（`STORAGE.md`「保留期限、Pulse 的 buffer.max_age_hours 與去重範圍」）。大量 Pulse 同時恢復連線時的寫入尖峰見 `STORAGE.md`「量測」。

**從 0.3 升級到 0.4：主控台 API 預設監聽 `127.0.0.1:9443`**。升級前確認 9443 沒有被其他程式使用（`ss -ltnp | grep :9443`），否則 Kairos 以結束代碼 11 無法啟動（systemd 每 5 秒重試）；被佔用時在 `kairos.toml` 加上 `[console]` 的 `listen`（換連接埠）或 `enabled = false`。資料庫升級到 schema v3（新增 `api_tokens` 與被拒憑證的欄位）：**升級前以 `kairos db backup` 備份**，升級後 0.3 無法開啟。檔案描述元的需要量多了主控台的連線上限（預設 64）；發佈包 unit 的 `LimitNOFILE=8192` 足夠。協定不變，Kairos 與 Pulse 0.4 可以任意順序、分批升級（Pulse 0.4 的變更只在本機：`pulse status`、狀態檔與日誌的事件代碼，見 Pulse 的 INSTALL 第 10 節）。

**從 0.4 升級到 0.5：註冊通道與 PKI（schema v4）**。資料庫升級到 schema v4（註冊 token、申請、簽發的憑證、CRL、設定、稽核等資料表）：**升級前以 `kairos db backup` 備份**，升級後 0.4 無法開啟。`kairos.toml` 沒有 `[enroll]` 時註冊通道維持關閉（啟動時記一次 `KRS-I2010`），沒有 `[pki]` 時為離線簽發，**行為與 0.4 相同**，不需要修改設定；要啟用時依附錄 P 的第 P.5 節（0.8.0 起的新安裝改用第 6 節的 `kairos pki init`）。啟用後檔案描述元的需要量另加 `enroll.max_connections`（預設 1024），發佈包 unit 的 `LimitNOFILE=8192` 足夠。既有的 API token 維持 `read`；寫入操作（核准、撤銷、模式切換）需要以 `--scope operate` 或 `--scope admin` 建立新的 token。主控台修改的註冊模式與憑證政策存在資料庫、優先於 `kairos.toml`，以 `kairos config show --effective` 查看來源。Pulse 0.4 照常以 8443 連線（`hello_ack` 與 0.4 相同）；註冊與 8443 的輪替需要 Pulse 0.5。

**從 0.5 升級到 0.6：告警、使用者與稽核、可觀測性（schema v6）**。資料庫依序升級到 schema v5（告警）與 v6（使用者、角色指派、工作階段、稽核雜湊鏈）：**升級前以 `kairos db backup` 備份**，升級後 0.5 無法開啟。資料目錄會多出 `alerts-secrets.key`、`console.key`（0600），之後的備份要一起保存（第 10.2 節）。既有的 API token **保留原本的範圍與權限**（`read`／`operate`／`admin`），**不會**得到 0.6 新增的權限（使用者、稽核、告警）：需要讀寫告警的整合以 `kairos api-token create --role viewer|operator|admin` 建立新 token；`kairos api-token create` 未指定時改為 `--role viewer`（主機、健康、告警事件與靜音的讀取），需要註冊、PKI 或設定的讀取時以 `--role auditor` 或 `--role operator` 建立。告警引擎預設啟用、沒有通知管道（不需要時 `[alerts] enabled = false`）。日誌行首的時間改為 RFC 3339（需要過渡時 `log.format = "legacy"`）；主控台的監聽上多了不需認證的 `/healthz`、`/readyz` 與只接受 loopback 的 `/metrics`（第 9.2a 節；不要時 `observability.on_console = false`）。既有的稽核紀錄在升級後第一次寫入時補上雜湊。升級前可先以新版執行 `kairos --check-config --config /opt/kairos/kairos.toml`；升級後依第 9.4 節建立首位管理員。協定不變，Pulse 可以任何順序升級。

**從 0.4 升級到 0.5 的順序：先升級 Kairos，再升級 Pulse**：(1) `kairos db backup` → (2) 換上新版 Kairos 並重新啟動（此時註冊通道仍關閉）→ (3) 建立簽發 CA 並在 `kairos.toml` 開啟 `[enroll]`、`[pki]`（第 P.5 節），重新啟動後確認日誌有「註冊端點（8444）已啟動」→ (4) 防火牆開放 8444，只允許要註冊的主機 → (5) 通知 Pulse 端升級（建立 `/opt/pulse/identity`、換上新版 `pulse.service`，Pulse 的 INSTALL 第 10 節）。`storage.backend = "jsonl"` 時沒有註冊服務（不監聽 8444）。完整步驟與版本組合見 `CHANGELOG.md` [0.5.0]。

**從 0.7 升級到 0.8（〔草稿〕）：指標擴充（schema v8）、套件、`kairos pki`、aarch64**。資料庫升級到 schema v8（計數器的速率與推算序列、主機資產、行程快照；第一次啟動時重建 `series`，大型資料庫會多花一些時間）：**升級前以 `kairos db backup` 備份**，升級後 0.7 無法開啟。容量估算由每台約 52 MB 改為約 129 MB：主機多、沿用預設 `storage.max_total_bytes` 的部署依 `STORAGE.md` 第 10 節調高上限或縮短保留天數。協定版本不變，其他設定不需要修改。0.8 新增的 deb／rpm 是另一種安裝方式：既有的 tar.gz 安裝**不要**直接改裝套件（套件的預設設定是全新安裝用的；兩者版面相同，但 `kairos.service` 會從 `/etc/systemd/system` 換到 `/usr/lib/systemd/system`），照下方的步驟換執行檔即可。新版 `kairos.service` 多了 `InaccessiblePaths=-/opt/kairos/root-ca`（`kairos pki init` 的根 CA 暫存目錄；目錄不存在時沒有影響），手動 PKI 的部署可以沿用舊 unit。**`auth.client_ca`（安全修正）**：0.8 起 8443 一律拒絕經過已退役簽發 CA 的用戶端憑證鏈，`auth.client_ca` 指向根 CA 或代理 CA 的既有部署不需要修改設定也受到保護（`PKI.md` 第 15.4 節第 1 項）。仍**建議**改為 `client_ca = "/opt/kairos/data/pki/issuing-cas.pem"`：以根 CA 當信任錨時，根 CA 底下任何其他 CA 簽出的用戶端憑證都會通過，且不受 Kairos 的 CRL 管理。改之前以 `kairos pki certificates list` 與主控台 S04 確認每台主機都已改用 Kairos 簽發的憑證；仍有手動簽發的憑證時，`auth.client_ca` 只保留那張舊代理 CA（不要放根 CA），等那些主機重新註冊後再改。

**換上新版 unit**：新版 `kairos.service` 新增 `RestartPreventExitStatus=2 3 4`（用法、設定、憑證錯誤時不再無限重試）。升級時以發佈包的 `kairos.service` 覆寫 `/etc/systemd/system/kairos.service`（自訂內容放在 `systemctl edit` 的 drop-in）後 `systemctl daemon-reload`。完整的變更與升級步驟見新版發佈包中的 `CHANGELOG.md`。

**從 0.1.x 升級時的順序：不必同時升級，建議先升級 Pulse、再升級 Kairos。**（0.2 → 0.3 見上方，先升級 Kairos。）協定 v2 以版本協商相容 v1：新舊版本混用時自動以協定 v1 通訊（線上格式與舊版相同），Kairos 與 Pulse 可以分批、以任何順序升級，混用期間照常運作；兩端都是新版後才使用協定 v2（通用指標、主機資訊）。仍建議先升級 Pulse 的理由：

- 舊版 Pulse（`44dcee5` 及更早）連新版 Kairos 時，主機名稱含新版 Kairos 禁止的字元會**無法註冊**，心跳間隔較大時也可能誤斷線；新版 Pulse 連舊版 Kairos 沒有這類問題（舊版 Kairos 的 `timeouts.write_secs` 維持預設時）。先升級 Pulse 就不會經過有風險的組合。
- 資料格式的改變與順序無關：Kairos 升級後 JSONL 每行多了 `schema`，新版 Pulse 連上後寫成 `schema` 2，讀取 JSONL 的工具要在升級 Kairos **前**改為依 `schema` 分流（`CHANGELOG.md` 破壞性變更第 6 項）。

確認協商結果：Kairos 的 agent 上線日誌結尾為「，協定 vN」，`data/agents.json` 每個 agent 的 `proto` 為 1 或 2；混用期間未升級的 Pulse 為 v1。各版本組合的細節見 `CHANGELOG.md`「版本相容矩陣」與 `README.md`「升級注意事項」。

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

# 3. 保留舊版以便回滾，換上新版（先備份資料庫：新版可能升級 schema；從 0.2 升級時還沒有資料庫，略過備份）
sudo -u kairos /opt/kairos/kairos db backup /var/backups/kairos/kairos-before-upgrade-$(date -u +%F).db \
  --config /opt/kairos/kairos.toml
cp -p /opt/kairos/kairos /opt/kairos/kairos.prev
install -o root -g root -m 755 pkg-new/kairos /opt/kairos/kairos.new
mv /opt/kairos/kairos.new /opt/kairos/kairos
install -o root -g root -m 644 pkg-new/kairos.example.toml pkg-new/INSTALL.md \
  pkg-new/README.md pkg-new/PROTOCOL.md pkg-new/STORAGE.md pkg-new/API.md pkg-new/PKI.md pkg-new/CHANGELOG.md \
  pkg-new/THIRD-PARTY-LICENSES /opt/kairos/
# unit 有變動時：
# install -o root -g root -m 644 pkg-new/kairos.service /etc/systemd/system/kairos.service
# systemctl daemon-reload

# 3a. （選用，從 0.2 升級）匯入 0.2 的 JSONL：先試執行估算筆數，停止 Kairos 後匯入，再啟動。
#     匯入期間獨占資料庫（Kairos 執行中時以結束代碼 11 拒絕）；速度約每秒 3～4 萬行。
#     停機要短時，可在換上新版之前（0.2 仍在執行）先以 pkg-new/kairos 匯入前幾天的檔案（--until），
#     這裡只匯入當天的（STORAGE.md「匯入舊的 JSONL」的「縮短停機」）
sudo -u kairos /opt/kairos/kairos reports import /opt/kairos/data --dry-run --config /opt/kairos/kairos.toml
systemctl stop kairos
sudo -u kairos /opt/kairos/kairos reports import /opt/kairos/data --config /opt/kairos/kairos.toml

# 4. 重新啟動並確認（第 9.1 節）
systemctl restart kairos
/opt/kairos/kairos --version
journalctl -u kairos -n 30 --no-pager
```

- 重新啟動時連線中的 Pulse 收到 `close_notify`，依退避在數秒內自動重連；不需要動 Pulse。
- 重啟後，主機清冊中最近 24 小時內出現過的主機以離線狀態出現在 `agents.json`，線上的 agent 重連後恢復上線（`storage.backend = "jsonl"` 時沒有清冊，`agents.json` 從空開始）。
- 回滾：`mv /opt/kairos/kairos.prev /opt/kairos/kairos && systemctl restart kairos`。新版升級過 schema 時，舊版會以「資料庫的 schema 版本 … 比本版 Kairos 支援的 … 新」拒絕啟動：停止服務後把升級前的備份還原成 `data/kairos.db`（`STORAGE.md`「備份與還原」）再啟動舊版；備份之後收到的 report 不在其中。

### 11.1 以套件安裝時的升級（0.8.0〔草稿〕起）

```bash
# 先驗證新版的 SHA256SUMS 與簽章（第 2 節），備份資料庫，再安裝新版套件
sudo -u kairos kairos db backup /var/backups/kairos/kairos-before-upgrade-$(date -u +%F).db   --config /opt/kairos/kairos.toml
apt install ./kairos-<新版本>-<commit>-linux-x86_64.deb       # 或 dnf install ./….rpm
```

套件升級時保留 `/opt/kairos/kairos.toml`（設定檔有修改時，dpkg 會詢問是否採用新版的預設值：選擇保留〔預設〕；rpm 把新版的預設值寫成 `kairos.toml.rpmnew`），`data/`、`logs/` 不動，執行中的服務自動重新啟動（`systemctl try-restart`）。回滾：`apt install ./kairos-<舊版本>-….deb`（`dnf downgrade ./….rpm`）並依上方「回滾」還原資料庫。

## 12. 移除

以套件安裝時：`apt remove kairos`（`dnf remove kairos`）停止並移除程式與 unit，`/opt/kairos/data`、`kairos.toml`、系統帳號保留；`apt purge kairos` 另外刪除 `kairos.toml` 與 `logs/`，**資料目錄仍保留**（資料庫與 `data/pki` 的私鑰），確定不再需要時自行 `rm -rf /opt/kairos` 並 `userdel kairos`。以發佈包安裝時：

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

離線保存的根 CA（手動 PKI 為管理主機上的 `~/kairos-pki/`）不會被刪除，依組織政策另行處理。

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
| 所有代理突然被拒，日誌出現 `ExpiredRevocationList` 或 `CRL … 已過期` | `data/pki/crl/issuing.crl`（線上簽發）過期：Kairos 沒有重新產生（檢查 `KRS-E1205`、`KRS-E1303`），以 `sudo -u kairos kairos pki gencrl` 立即產生。`auth.crl`（手動 PKI）過期：依第 P.4 節重新產生並依第 10.1 節部署，之後設定第 10.3 節的 `kairos-crl.timer` |
| `[WARN] CRL … 將在 … 後過期` | CRL 即將過期：依第 6.4、10.1 節更新；使用 `kairos-crl.timer` 時檢查 `journalctl -u kairos-crl` |
| `[WARN] 伺服器憑證 … 將在 … 後到期` | 依第 10.4 節以 `kairos pki sign` 重新簽發（手動 PKI 依第 P.1 節），依第 10.1 節覆寫 |
| `無法啟用 WAL（journal_mode 為 …）` | `data/` 所在的檔案系統不支援 SQLite 的 WAL（例如 NFS、SMB）。改用本機磁碟 |
| `資料庫的 schema 版本 N 比本版 Kairos 支援的 M 新` | 降版執行。改用較新的版本，或停止服務後還原升級前的備份（第 11 節） |
| `不是 Kairos 的資料庫（application_id = …）` | `storage.data_dir` 指到其他程式的 SQLite 檔。確認設定 |
| `SQLite 資料庫已使用 … bytes，超過 storage.max_total_bytes` | 已達總容量上限，Kairos 正在刪除最舊的原始 report（之後是彙總）。依 `STORAGE.md`「容量估算」調高上限或縮短保留天數 |
| `kairos hosts list` 回 `KRS-E4003`，原因為權限不足 | 以 root 或服務帳號執行：`sudo -u kairos /opt/kairos/kairos hosts list --config /opt/kairos/kairos.toml` |
| `kairos reports import` 回 `KRS-E4304`「資料庫正在使用中」（結束代碼 11） | Kairos 正在執行（或另一個 `kairos` 子命令正在使用資料庫）。`systemctl stop kairos` 後再匯入；`--dry-run` 不需要停止 |
| 匯入期間 Kairos 啟動失敗（`database is locked`） | `kairos reports import` 正獨占資料庫。匯入結束後 systemd 會依 `Restart=on-failure` 自動重新啟動；匯入時請先停止服務 |
| `kairos reports import` 以 root 執行時以結束代碼 10 拒絕 | 匯入會建立或寫入 `kairos.db`，請以服務帳號執行：`sudo -u kairos /opt/kairos/kairos reports import …` |
| 其他錯誤訊息 | 見 `README.md`「疑難排解」 |

## 附錄 P：以 openssl 手動管理 PKI（0.7 以前的部署）

0.8.0 起新安裝改用第 6 節的 `kairos pki init`。以下是 0.7 以前的步驟，留給沿用既有 CA 的部署、或不讓任何 CA 私鑰放在伺服器上的離線簽發模式（`PKI.md` 第 6 節）。其中的 openssl 指令可以用 `kairos pki sign`（伺服器憑證、代理簽發 CA、用戶端憑證）與 `kairos pki gencrl`（CRL）取代，免寫 `ca.cnf`（`PKI.md` 第 15 節）。


正式環境需要兩個 CA，用途不同、請分開建立：

| CA | 用途 | 管理方式 |
|---|---|---|
| 伺服器 CA（`ca.pem`） | 簽發 Kairos 的伺服器憑證；每台 Pulse 只信任它 | `openssl x509 -req`（伺服器憑證不需撤銷，到期重簽即可） |
| 代理 CA（`client-ca.pem`） | **只**用來簽發 Pulse 的客戶端憑證（mTLS） | `openssl ca` 資料庫，才能撤銷並產生 CRL |

兩把 CA 私鑰（`ca.key`、`client-ca.key`）都在管理主機上產生並離線保存，**不要**複製到 Kairos 主機。以下指令在管理主機執行。

### P.1 伺服器 CA 與伺服器憑證

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

- `cert.pem`、`key.pem` 交給 Kairos（第 P.6 節）。
- `ca.pem` 交給每台 Pulse（`server.ca_cert`），Pulse 的 `server.domain` 填 SAN 中的名稱。
- `basicConstraints=critical,CA:FALSE` 不能省：rustls 不接受 CA 憑證當伺服器憑證（`CaUsedAsEndEntity`）。

### P.2 代理 CA（openssl ca 資料庫，可撤銷）

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

- `client-ca.pem` 與 `agents.crl` 交給 Kairos（第 P.6 節）；`client-ca.key` 與整個 `agent-ca/` 目錄離線保存並備份（`index.txt` 遺失就無法撤銷先前簽發的憑證）。
- 代理 CA 只用來簽發代理的客戶端憑證：Kairos 信任它簽發的**任何**客戶端憑證。

### P.3 簽發代理的客戶端憑證

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

### P.4 撤銷與定期更新 CRL

```bash
cd ~/kairos-pki/agent-ca
openssl ca -batch -config ca.cnf -revoke "issued/$AGENT_ID.pem" -crl_reason keyCompromise
openssl ca -batch -config ca.cnf -gencrl -crlexts crl_ext -out agents.crl
```

把新的 `agents.crl` 部署到 Kairos（第 10.1 節）。Kairos 在下一個重載週期（`server.reload_interval_secs`，預設 30 秒）載入，被撤銷的代理收到 `certificate_rejected` 並被斷線，之後重連在 TLS 握手階段就被拒；其他代理不受影響。

> **CRL 過期會讓所有代理被拒。** Kairos 對 CRL 採 fail closed：超過 `nextUpdate`（上面設定為 30 天）就視為無法判定撤銷狀態。請在到期前定期重新產生並部署，例如每週一次（即使沒有新的撤銷；第 10.3 節的 `kairos-crl.timer`）。`openssl crl -in agents.crl -noout -nextupdate` 可查看到期時間。Kairos 在 `nextUpdate` 前 7 天（`auth.crl_expiry_warning_days`）起記 `WARN`，過期後記 `ERROR`。

### P.5 （選用，0.5.0）註冊通道與線上簽發 CA

註冊通道（8444，`crates/kairos-proto/ENROLL.md`）讓 Pulse 以安裝字串自行申請憑證，取代第 P.3 節的逐台手動簽發。Kairos 持有一把**簽發 CA** 的私鑰（`data/pki/issuing-<ca_id>.key`，0600），核准後立即簽發（線上模式；架構與取捨見 `PKI.md`）。簽發 CA 由第 P.2 節的代理 CA（或另外的離線根 CA）簽發，代理 CA 的私鑰仍不離開管理主機。

```bash
# 1. Kairos 主機：產生簽發 CA 的私鑰與 CSR（預設寫到 data/pki/issuing-<ca_id>.csr）
sudo -u kairos /opt/kairos/kairos pki issuing-ca new --config /opt/kairos/kairos.toml
# 把 /opt/kairos/data/pki/issuing-1.csr 帶到管理主機的 ~/kairos-pki/agent-ca/

# 2. 管理主機：以代理 CA 簽發（CA:TRUE、pathlen:0、keyCertSign／cRLSign、EKU clientAuth，5 年）
cd ~/kairos-pki/agent-ca
cat > issuing_ca_ext.cnf <<'EOF'
basicConstraints = critical,CA:TRUE,pathlen:0
keyUsage = critical,keyCertSign,cRLSign
extendedKeyUsage = clientAuth
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid
EOF
openssl x509 -req -in issuing-1.csr -CA client-ca.pem -CAkey client-ca.key -CAcreateserial   -days 1825 -extfile issuing_ca_ext.cnf -out issuing-1.pem
# 把 issuing-1.pem 帶回 Kairos 主機

# 3. Kairos 主機：安裝（檢查與私鑰成對、擴充欄位、至少剩 2 年；第一個簽發 CA 自動啟用）
sudo -u kairos /opt/kairos/kairos pki issuing-ca install issuing-1.pem --config /opt/kairos/kairos.toml
sudo -u kairos /opt/kairos/kairos pki issuing-ca list --config /opt/kairos/kairos.toml
```

`kairos.toml` 加上（完整欄位見 `kairos.example.toml` 的 `[enroll]`、`[pki]`）：

```toml
[enroll]
mode = "open"                 # 也可以之後以 kairos enroll mode open 或主控台切換

[pki]
mode = "online"
server_ca_path = "ca.pem"     # Pulse 信任的 CA（第 P.1 節的伺服器 CA）；安裝字串的 CA 指紋
```

- 依路徑調整：上例在第 P.2 節的 `agent-ca/` 目錄執行（代理 CA 的 `client-ca.pem`、`client-ca.key`）；`ca.pem` 以 644 放到 `/opt/kairos/`。
- 防火牆：允許要註冊的主機連到 8444（`ufw allow proto tcp from 10.0.0.0/16 to any port 8444`；firewalld 同第 9.3 節的 rich rule，改 port 8444）。8444 不需要對管理網段以外開放；不開放新註冊時以 `kairos enroll mode renew_only`（只受理過期續簽）或 `closed` 關閉。
- 建立註冊 token：`sudo -u kairos /opt/kairos/kairos enroll-token create --name rollout --group <群組> --max-uses 50 --config /opt/kairos/kairos.toml` 建立 token（安裝字串寫在 stderr）；在 Pulse 主機執行 `pulse enroll --install-string <安裝字串>`。待核准的申請以 `kairos enroll list`、`kairos enroll approve <request_id>` 處理（或主控台，需要 `operate` 範圍的 API token）。
- Kairos 自己產生簽發 CA 的 CRL（`data/pki/crl/issuing.crl`，每 24 小時與每次撤銷），撤銷以 `kairos pki revoke <序號或 agent_id> --reason <原因> --comment <說明>`，不必經過第 P.4 節的 `openssl ca`。
- 不想讓私鑰放在伺服器上時採離線模式（`pki.mode = "offline"`）：核准後以 `kairos pki queue export` 匯出 CSR，在管理主機 `kairos pki sign-bundle` 簽發後 `kairos pki queue import` 匯入（`PKI.md` 第 6 節）。

### P.6 放置憑證

把以上產生的檔案帶到 Kairos 主機（例如 `scp` 到 `~/kairos-files/`），再放到 `/opt/kairos`：

```bash
cd ~/kairos-files
install -o root -g kairos -m 644 cert.pem       /opt/kairos/cert.pem
install -o root -g kairos -m 640 key.pem        /opt/kairos/key.pem
install -o root -g kairos -m 644 client-ca.pem  /opt/kairos/client-ca.pem
install -o root -g kairos -m 644 agents.crl     /opt/kairos/agents.crl

```

`kairos.toml` 的 `[server]` 改為 `cert_path = "cert.pem"`、`key_path = "key.pem"`，`[auth]` 改為 `client_ca = "client-ca.pem"`、`crl = ["agents.crl"]`。
