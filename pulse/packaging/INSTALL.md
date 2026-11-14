# Pulse 安裝指南

本文件給地端管理者使用：把 Pulse 安裝成**系統服務**（Linux 為 systemd、Windows 為服務 `Pulse`），連到中央伺服器 Kairos。功能、設定欄位與協定細節請見同目錄的 `README.md` 與 `PROTOCOL.md`。

- **第 A 節（建議，0.8.0 起）**：Linux 以 deb／rpm 套件或一行安裝腳本安裝，Windows 以 MSI 安裝；都以 Kairos 管理者提供的**安裝字串**（`pulse enroll`）自動取得憑證。
- **第 0～12 節**：從 tar.gz 發佈包手動安裝（不使用套件管理員、多實例、手動 CSR 等），以及共通的維運、升級與移除。

> 不要用 `systemd-run --user` 或使用者層級的 unit 跑正式服務：沒有開啟 linger 時，最後一個登入 session 結束，user manager 會連同 Pulse 一起被停掉。

## A. 以套件或安裝腳本安裝（0.8.0）

需要 Kairos 0.5 以上、註冊端點（8444）為「開放」，以及 Kairos 管理者給你的**安裝字串**（`pulse-enroll:v1:…`，含 Kairos 位址、註冊 token 與 CA 指紋；`kairos enroll-token create` 或主控台「註冊與憑證 › 註冊 token」產生）。私鑰在本機產生、不會離開這台主機。

### A.1 一行安裝腳本（Linux）

發佈處的 `pulse-install.sh`（也列在簽過章的 `SHA256SUMS` 中）依序：驗證 `SHA256SUMS` 的簽章 → 依本機架構（x86_64／aarch64）與套件管理員（dpkg／rpm）選出套件、核對 SHA-256 → 安裝 → `pulse enroll` → 啟動服務。

```bash
# 發佈公鑰 allowed_signers 必須從另一個管道取得（第 2 節），事先放在主機上，例如 /etc/kairos-pulse/allowed_signers。
# 先核對腳本本身：雜湊取自已驗證的 SHA256SUMS（或供應商另外提供的管道）
curl -fsSLO https://downloads.example.com/kairos-pulse/0.8.0/pulse-install.sh
echo "<pulse-install.sh 的 SHA-256>  pulse-install.sh" | sha256sum -c -
# 安裝字串以環境變數傳入（不留在 shell 歷史）；--dir <目錄> 可改用已下載好的發佈檔（離線環境）
sudo PULSE_INSTALL_STRING='pulse-enroll:v1:eyJzIjoia2Fpcm9z…' sh pulse-install.sh \
  --base-url https://downloads.example.com/kairos-pulse/0.8.0 \
  --allowed-signers /etc/kairos-pulse/allowed_signers
```

| 結束代碼 | 意義 |
|---|---|
| 0 | 已註冊並啟動（`pulse status --brief` 為 `online`） |
| 7 | 已安裝並送出申請，等待 Kairos 管理者核准（畫面上有**申請編號**，請告訴管理者）；服務已啟動，核准後自動連線 |
| 2／3 | 用法錯誤（例如沒有 `--allowed-signers`）／不支援的系統（架構、沒有 dpkg 或 rpm、沒有 systemd） |
| 4 | 簽章或 SHA-256 驗證失敗：**不要安裝**，向供應商確認 |
| 5／6 | 下載失敗／套件安裝失敗 |
| 其他 | `pulse enroll` 的結束代碼（`pulse enroll --help`；例如 6 token 無效或被拒） |

- 腳本需要 `curl` 或 `wget`、`sha256sum`、OpenSSH 8.1 以上的 `ssh-keygen`。RHEL／Rocky 8 的 OpenSSH 8.0 沒有 `ssh-keygen -Y verify`：在管理主機驗證後以 `--dir` 安裝，或手動依 A.2 安裝。
- **從 Kairos 的註冊端點下載**（Kairos 0.9 起）：Kairos 管理者把同一個發佈版本的 `pulse-install.sh`、套件、`SHA256SUMS`、`SHA256SUMS.sig` 放在 `enroll.download_dir` 後，主控台「建立 token」會顯示一行指令：

  ```bash
  curl -fsSLO --cacert kairos-ca.pem https://kairos.example.com:8444/agent/pulse-install.sh && \
    sudo PULSE_INSTALL_STRING='pulse-enroll:v1:…' sh pulse-install.sh \
      --base-url https://kairos.example.com:8444/agent --ca-file kairos-ca.pem \
      --allowed-signers /etc/kairos-pulse/allowed_signers
  ```

  `kairos-ca.pem` 是 Kairos 的根 CA（`pki.server_ca_path`），8444 的伺服器憑證由它簽發、不在系統的信任清單中；`--ca-file` 讓 curl／wget 以它驗證 HTTPS。套件的完整性仍以發佈公鑰（`--allowed-signers`）驗證 `SHA256SUMS.sig`，不依賴 HTTPS 或 Kairos。根 CA 與發佈公鑰都要從另一個管道取得。
- 已安裝過的主機再執行：升級套件（保留 `pulse.toml`、`identity/`、`agent.id`），已註冊時不重新註冊。只安裝不註冊：`--no-enroll`；不啟動服務：`--no-start`。
- **遠端管理（0.9.0）**：腳本另外把 `--allowed-signers` 的檔案複製到 `/etc/kairos-pulse/allowed_signers`（root 644），作為遠端升級的信任錨（已經放在那裡時不動）。不接受 Kairos 推送的升級：`--no-remote-upgrade`；不接受下發的設定：`--no-remote-config`（兩者都寫進 `/opt/pulse/pulse.toml`，見第 10.1 節）。
- 安裝字串會短暫出現在 `pulse enroll` 的命令列（`ps` 看得到）；token 有使用次數與期限，部署完成後由管理者停用（`kairos enroll-token disable`）。

### A.2 deb／rpm 套件（Linux）

```bash
# 依第 2 節驗證 SHA256SUMS 的簽章與套件的雜湊後：
apt install ./pulse-<版本>-<commit>-linux-x86_64.deb     # Debian／Ubuntu（aarch64 為 linux-aarch64）
dnf install ./pulse-<版本>-<commit>-linux-x86_64.rpm     # RHEL／Rocky／Alma
sudo pulse enroll --install-string-file - < install-string.txt   # 安裝字串存成檔案（0600）；需人工核准時顯示申請編號；Ctrl+C 不會遺失申請
sudo systemctl start pulse
pulse status
```

- **安裝字串含註冊 token，不要放在命令列**（0.8.0）：`--install-string <字串>` 在 `pulse enroll` 執行期間其他帳號可從 `ps`／`/proc/<pid>/cmdline` 看到，也會留在 shell 歷史。建議存成只有 root 能讀的檔案後 `--install-string-file <檔案>`，或從 stdin 傳入 `--install-string-file -`（例如 `sudo pulse enroll --install-string-file - < install-string.txt`）；`--install-string` 保留相容但不建議。一行安裝腳本與 Ansible role 都經 stdin 傳入。
- 套件的版面與 tar.gz 相同（`/opt/pulse`），另外安裝 `/usr/lib/systemd/system/pulse.service` 與 `/usr/bin/pulse`。`/opt/pulse/pulse.toml` 是初始設定（只有註解；`pulse enroll` 會加上 `[server]`），升級不覆寫。
- **套件做的事**（`packaging/pkg/*.sh`）：安裝前建立系統帳號 `pulse`；安裝後建立 `logs/`、`buffer/`（`pulse:pulse` 750）、`identity/`（700），產生 `agent.id`（`root:pulse` 640；已存在時沿用），`systemctl enable pulse`（不啟動）；升級時 `systemctl try-restart pulse`；移除時 `systemctl disable --now pulse`。0.9.0 起另外安裝 `/usr/lib/systemd/system/pulse-upgrade.path`／`pulse-upgrade.service` 並啟用 `pulse-upgrade.path`（遠端升級的輔助程式，第 10.1 節），寫入 `/opt/pulse/.install-method`（`deb`／`rpm`，供遠端升級選擇發佈檔）；移除時一併停用 `pulse-upgrade.path`。
- **移除**：`apt remove pulse`／`dnf remove pulse` 保留身分（`identity/`、`agent.id`）、設定與日誌，重新安裝後沿用（rpm 會把改過的設定改名為 `pulse.toml.rpmsave`，重新安裝後改回 `pulse.toml`）；`apt purge pulse` 另外刪除身分、緩衝、日誌與 `/var/lib/pulse`。**完全移除前先通知 Kairos**（`cd /opt/pulse && ./pulse decommission --initiated-by package_manager`，第 12 節）：套件的移除腳本不會自動執行，避免暫時移除（例如重新安裝）就撤銷憑證。
- 多實例（第 9 節）不支援套件安裝：以 tar.gz 依第 9 節設定。

### A.3 Windows（MSI）

`pulse-<版本>-<commit>-windows-x86_64.msi`（另有內容相同的 `.zip`）。以系統管理員身分：

```powershell
# 依第 2 節驗證 SHA256SUMS（Windows 11 內建的 ssh-keygen、Get-FileHash -Algorithm SHA256）後：
msiexec /i pulse-<版本>-<commit>-windows-x86_64.msi /qn /l*v pulse-install.log
Get-Content .\install-string.txt | & "C:\Program Files\Pulse\pulse.exe" enroll --install-string-file -
sc.exe query Pulse            # 應為 RUNNING：MSI 安裝後會啟動服務，pulse enroll 成功後會重新啟動它；不是時 sc.exe start Pulse
& "C:\Program Files\Pulse\pulse.exe" status
```

- 安裝到 `C:\Program Files\Pulse\`：`pulse.exe`、文件、初始的 `pulse.toml`（安裝目錄**不**加入 PATH）；註冊服務 **`Pulse`**（自動啟動、LocalSystem，參數 `service run`）。服務的工作目錄是安裝目錄（**不是** `C:\Windows\System32`），`pulse.toml`、`agent.id`、`identity\`、`buffer\` 與日誌 `logs\pulse-YYYY-MM-DD.log` 都在這裡；本機狀態檔為 `%ProgramData%\Pulse\status.json`。
- **失敗復原**：服務異常結束（含以非 0 結束代碼停止）時由服務控制管理員重新啟動：第 1 次失敗後等 10 秒、第 2 次 60 秒、第 3 次起每 300 秒，**沒有次數上限**；連續一天沒有失敗才重新從 10 秒算起。設定或憑證錯誤時服務也會每 300 秒重試一次，修好之後不必手動啟動。服務每次啟動時會把這項設定校正回來（`sc.exe qfailure Pulse` 查看；自訂的失敗動作會被改回）。0.8.0～0.10.1 的文件寫「一天最多 3 次」是錯的（實際上每 10 秒重新啟動、沒有上限）。由 MSI 註冊的服務在第一次啟動時才設定失敗復原，其中「非 0 結束代碼也算失敗」依 Microsoft 的文件要到下次開機才生效。
- **狀態目錄 `%ProgramData%\Pulse`**（狀態檔 `status.json`、集中設定的快取 `remote\config.json`）：`%ProgramData%` 的預設 ACL 讓一般使用者可以建立子目錄，所以服務在使用這個目錄之前會檢查它——擁有者必須是 SYSTEM、Administrators 或 TrustedInstaller，DACL 不能讓其他主體寫入，而且不能是 junction 或符號連結。目錄不存在時，服務以只有 SYSTEM 與 Administrators 可寫（Users 只能讀取目錄本身）的 ACL 建立它；MSI 不建立這個目錄（wixl 不支援設定權限）。**不合規時服務照常執行但不使用它**：狀態檔改寫在安裝目錄的 `logs\status.json`、設定快取改放 `logs\remote\`，日誌與 `pulse status` 的「設定」列顯示 `PLS-E8207`。**0.8.0～0.10.1 建立的目錄繼承了 `%ProgramData%` 的 ACL（Users 可在其中建立檔案），升級後也會被拒絕**，請以系統管理員執行一次：

  ```powershell
  sc.exe stop Pulse
  cmd /c rd /s /q "%ProgramData%\Pulse"     # 其中只有狀態檔與設定快取，都會重建；rd 不會跟隨 junction
  sc.exe start Pulse
  icacls "$env:ProgramData\Pulse"           # 應只有 SYSTEM、Administrators 的 (F) 與 Users 的 (RX)
  ```

  `status.file` 明確指定的路徑不做這項檢查（由管理者負責它的 ACL）。
- **服務的啟動**：MSI 在安裝、升級與修復（`msiexec /fa`）結束時啟動服務（只送出啟動要求、不等它連上 Kairos；服務啟動失敗不會讓安裝失敗）。第一次安裝、還沒有執行 `pulse enroll` 時服務也會啟動：它會連預設的 `127.0.0.1:8080` 並持續重試，`pulse enroll` 寫好設定後會重新啟動它。0.8.0～0.10.1 的 MSI 只會停止服務、不會再啟動：**從這些版本升級、或以它們修復之後，服務會停到下次開機或手動 `sc.exe start Pulse`**（ADR-0001 的 X4）。這一點只確認了 MSI 的內容（`ServiceControl` 表），尚未在 Windows 上實測。
- 升級：安裝新版 MSI（MajorUpgrade 取代舊版；`pulse.toml` 與執行時產生的檔案保留）。移除：`msiexec /x …` 或「設定 › 應用程式」：停止並刪除服務、刪除安裝的檔案；`pulse.toml`、`agent.id`、`identity\`、`buffer\`、`logs\` 保留。完全移除前先執行 `pulse decommission --initiated-by msi`。
- 不使用 MSI 時（zip）：解開到任一目錄，以系統管理員執行 `pulse.exe service install --start`（`pulse service --help`）；移除為 `pulse.exe service uninstall`。
- **0.8.0 的限制**：MSI 與 Windows 服務只在 Linux 上交叉編譯與產生，**尚未在 Windows 實機驗證**；MSI 沒有安裝字串的參數（安裝後另外執行 `pulse enroll`）、移除時不會自動 `pulse decommission`、不寫 Windows 事件記錄、沒有程式碼簽章（Authenticode）。`logs\` 沿用安裝目錄的 ACL（Users 可讀）。

### A.4 Ansible（最小版，0.8.0）

monorepo 的 `packaging/ansible/`（role `pulse` 與範例 `site-pulse.yml`）做與一行安裝腳本相同的事：在控制機以發佈公鑰驗證 `SHA256SUMS` 的簽章 → 依每台主機的架構與套件格式選出套件、複製並核對 SHA-256 → `apt`／`dnf` 安裝 → `pulse enroll`（已有申請或身分時略過）→ 啟用並啟動服務。

```bash
# 控制機：~/kairos-pulse-dist/0.8.0 放同一次發佈的 SHA256SUMS、SHA256SUMS.sig 與 pulse 的 deb／rpm；
# 安裝字串放在 Ansible Vault（group_vars/all/vault.yml 的 pulse_install_string）
ansible-playbook -i inventory.ini packaging/ansible/site-pulse.yml --ask-vault-pass   -e pulse_dist_dir=$HOME/kairos-pulse-dist/0.8.0 -e pulse_allowed_signers=$HOME/allowed_signers
```

- 變數：`pulse_dist_dir`、`pulse_allowed_signers`（必填）、`pulse_version`（目錄中有多個版本時）、`pulse_install_string`、`pulse_start`（預設 `true`）。`pulse enroll` 的工作以 `no_log` 執行，安裝字串不會出現在 Ansible 的輸出。
- 結果：已註冊（0）、等待核准（7，申請編號在主控台「註冊申請」）、已註冊過（11）都視為成功。
- 只經過 `ansible-playbook --syntax-check` 與套件選擇邏輯的測試（ansible-core 2.21），**尚未對實際主機執行**；沒有 Windows（MSI）的支援。

## 0. 發佈包內容

每個版本的發佈檔（0.8.0 起）：

| 檔案 | 說明 |
|---|---|
| `pulse-<版本>-<commit>-linux-<arch>.tar.gz` | 發佈包（`<arch>` 為 `x86_64` 或 `aarch64`；本節以下的內容） |
| `pulse-<版本>-<commit>-linux-<arch>.deb`、`.rpm` | 套件（第 A.2 節） |
| `pulse-<版本>-<commit>-windows-x86_64.msi`、`.zip` | Windows（第 A.3 節） |
| `pulse-<版本>-<commit>-<os>-<arch>.cdx.json` | 各平台的 SBOM |
| `pulse-install.sh` | 一行安裝腳本（第 A.1 節） |
| `SHA256SUMS`、`SHA256SUMS.sig` | 與 Kairos 共用的雜湊清單與簽章（第 2 節） |

發佈包 `pulse-<版本>-<commit>-linux-<arch>.tar.gz` 內含一層 `pulse-<版本>-<commit>-linux-<arch>/` 目錄：

| 檔案 | 權限 | 說明 |
|---|---|---|
| `pulse` | 755 | 執行檔（x86_64 或 aarch64、musl 靜態連結，不依賴系統函式庫） |
| `pulse.example.toml` | 644 | **內建預設值的參考**（見第 6 節），不需要複製或修改 |
| `pulse.service` | 644 | systemd unit 範本（單一實例） |
| `INSTALL.md` | 644 | 本文件 |
| `README.md`、`PROTOCOL.md` | 644 | 說明文件與協定規格 |
| `ERROR-CODES.md` | 644 | 錯誤代碼與日誌事件的對照（`pulse explain <代碼>` 指向這裡） |
| `pulse-alerts.example.yml` | 644 | Prometheus 告警規則範例（第 7.2 節，0.6.0） |
| `CHANGELOG.md` | 644 | 變更紀錄；升級前請先看「破壞性變更與升級步驟」 |
| `THIRD-PARTY-LICENSES` | 644 | 執行檔內含的第三方套件清單與授權全文（建置時由 `cargo about` 依 `Cargo.lock` 產生） |
| `pulse.cdx.json` | 644 | SBOM（CycloneDX 1.5 JSON，建置時由 `cargo cyclonedx` 產生）；發佈處另附一份同內容的 `pulse-<版本>-<commit>-linux-x86_64.cdx.json`，可直接匯入 Dependency-Track 等工具 |

檔名中的 `<版本>` 是 SemVer 版本號（與 `--version` 相同，版本規則見 `CHANGELOG.md`），`<commit>` 是建置時的 git commit；檔名帶 `-dirty` 表示從有未提交變更的工作目錄建置，**不應**用於正式環境。

以下範例的路徑與名稱：

| 項目 | 值 |
|---|---|
| 安裝目錄 | `/opt/pulse`（擁有者 `root:pulse`，751：其他使用者只能執行已知名稱的檔案，例如 `pulse status`，不能列出或讀取設定與私鑰） |
| 服務使用者 | `pulse`（系統帳號，不能登入） |
| 可寫目錄 | `/opt/pulse/logs`、`/opt/pulse/buffer`（本機緩衝；擁有者 `pulse`）；`/var/lib/pulse`（本機狀態檔，由 unit 的 `StateDirectory=pulse` 建立，0755） |
| Kairos | `monitor.example.com:8080`（換成 Kairos 管理者提供的位址與憑證名稱） |

**從第 2 節起，請在同一個 root shell 中依序執行**：

```bash
sudo -i          # 之後的指令都在這個 shell 中執行；本文件的 ~ 指 root 的家目錄（/root）
```

不能只在每個指令前加 `sudo`：

- `/opt/pulse` 的權限是 751（第 4 節），一般帳號無法列出或 `cd` 進去後建立檔案，而 `sudo cd` 不存在。
- 第 5 節的 `agent.id`、`client.key` 與 CSR 都寫到**目前目錄**，必須先 `cd /opt/pulse`；在別的目錄執行會把 `agent.id` 寫到錯的地方，服務啟動時讀不到（`/opt/pulse` 在服務中唯讀，無法自行建立）。
- 第 5 節設定的 `AGENT_ID`、`umask` 只在同一個 shell 中有效。
- 讀取 `logs/` 等確認步驟也需要 root（一般帳號會 `Permission denied`）；`/opt/pulse/pulse --version` 與 `pulse status` 一般帳號即可執行。

中途離開了 root shell 時，重新 `sudo -i`，並從該節開頭（含 `cd`）重新執行。

## 1. 需求

- Linux x86_64 或 aarch64（0.8.0 起）、systemd（建議 247 以上）；Windows 見第 A.3 節
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

install -d -o root  -g pulse -m 751 /opt/pulse
install -d -o pulse -g pulse -m 750 /opt/pulse/logs
install -d -o pulse -g pulse -m 750 /opt/pulse/buffer
install -d -o pulse -g pulse -m 700 /opt/pulse/identity   # 以 pulse enroll 註冊時（第 5.0 節）

cd ~/pulse-dist/pkg
install -o root -g root -m 755 pulse /opt/pulse/pulse
install -o root -g root -m 644 pulse.example.toml INSTALL.md README.md PROTOCOL.md \
  CHANGELOG.md ERROR-CODES.md THIRD-PARTY-LICENSES /opt/pulse/
```

`/opt/pulse` 本身屬於 root，pulse 使用者只能讀取執行檔、設定與憑證；其他使用者（751）只能執行 `/opt/pulse/pulse`（例如 `pulse status`），不能列出目錄，也讀不到 640 的 `pulse.toml`、`agent.id`、`client.key` 與 750 的 `logs/`、`buffer/`。unit 以 `ProtectSystem=strict` 讓服務只能寫 `logs/`、`buffer/` 與 systemd 建立的 `/var/lib/pulse`（本機狀態檔，第 8.2 節；不需要手動建立）。**`logs/` 必須在啟動前存在**，否則 unit 以 `226/NAMESPACE` 啟動失敗。`buffer/` 是本機緩衝（斷線或 Kairos 維護期間的資料暫存在這裡，Kairos 確認後才刪除，見第 6.2 節）；沒有建立時服務仍會啟動，但 Pulse 無法寫入緩衝，記 `ERROR` 後改為 at-most-once（斷線期間的資料不補送）。

## 5. 產生 agent_id 並申請客戶端憑證

Kairos 0.5 起有兩種方式：**以 `pulse enroll` 自動註冊（第 5.0 節，建議）**，或沿用手動 CSR（第 5.1～5.3 節，Kairos 0.4 以前或沒有開放註冊端點時）。

### 5.0 以 pulse enroll 註冊（Kairos 0.5 以上）

前提：Kairos 的註冊端點（預設 8444）為「開放」，管理者已在主控台「註冊與憑證 › 註冊 token」建立 token，並給你**安裝字串**（`pulse-enroll:v1:…`，含 Kairos 位址、token 與 CA 指紋），或 Kairos 位址、token 與 CA 指紋三項。

```bash
# 1. agent_id（服務帳號要讀得到；/opt/pulse 本身唯讀，必須在這裡先產生）
cd /opt/pulse
./pulse --print-agent-id
chown root:pulse agent.id && chmod 640 agent.id

# 2. 註冊（私鑰在本機產生、不會離開這台主機）
./pulse enroll --install-string-file - < install-string.txt   # 安裝字串（pulse-enroll:v1:…）存成檔案；不建議放在命令列（ps 看得到）
#   或：./pulse enroll --server kairos.example.gov.tw --token KRT1-7G4Q-M2XD-9PBA-H6WN \
#              --ca-pin sha256:3f9a1c07b2d48e610a9c55f27d13c8a0e4b729610f3d8ac291e56b04d7afe21b
```

- 需人工核准的 token：畫面顯示**申請編號**（例如 `7KQ2-M9XD`），請把它告訴管理者核准。終端機上預設等待 10 分鐘（`--wait <秒>`、`--no-wait` 可調）；時間到或按 Ctrl+C 都不會遺失申請，安裝服務（第 7 節）後由服務在背景接續，核准後自動連線。
- 完成後 `identity/` 有 `client.key`（0600）、`client.pem`、`ca.pem` 與 `enrollment.json`（0600，含輪詢憑據），`pulse.toml` 的 `[server]` 已寫入位址、`client_cert`、`client_key`、`ca_cert` 與 `use_system_roots = false`（原檔備份為 `pulse.toml.bak-<時間>`）；跳過第 6 節的手動設定，直接到第 6.1 節確認權限與第 7 節。
- 註冊端點不在 Kairos 位址的 8444 時：`--enroll-server host:port`（或只改連接埠 `--enroll-server :9444`），會另外寫入 `pulse.toml` 的 `[enroll]`（過期寬限期續簽使用）。
- 沒有 CA 指紋：互動模式會顯示指紋請你與主控台核對；非互動（部署工具）以 `PLS-E1406` 結束，確定要信任時加 `--trust-on-first-use`（主控台會標示「未驗證伺服器身分」）。
- 常見結果與結束代碼：0 已註冊；7 等待核准或排隊中（服務接續）；6 被拒絕（token 無效／過期／用完、申請被拒、註冊端點不受理新註冊 `PLS-E2008`）；5 無法連線或伺服器身分驗證失敗；11 已註冊（重新註冊加 `--force`）。完整說明見 Pulse README「自動註冊」與 `ERROR-CODES.md`。
- 本機時鐘與 Kairos 相差超過 5 分鐘時 Kairos 暫不受理（`PLS-E8302`）：校正時間（`timedatectl set-ntp true`）後服務會自動重送，不需要新的 token。
- 取消等待中的申請：`./pulse enroll --cancel`（向 Kairos 撤回後刪除本機未使用的私鑰）。

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

Kairos 有多個節點（高可用部署，`kairos/HA.md`）而前面沒有負載平衡器或虛擬 IP 時，另外加上依序改試的備用位址（見 6.3）：

```toml
fallback_addresses = ["kairos-b.example.com", "10.0.20.11:8443"]
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
| `/opt/pulse` | `root:pulse` | 751（其他使用者只能執行已知名稱的檔案） |
| `pulse`（執行檔） | `root:root` | 755 |
| `pulse.toml` | `root:pulse` | 640 |
| `ca.pem`、`client.pem` | `root:pulse` | 644 |
| `client.key`、`agent.id` | `root:pulse` | 640 |
| `logs/` | `pulse:pulse` | 750 |
| `buffer/`（Pulse 建立的檔案為 640） | `pulse:pulse` | 750 |
| `/var/lib/pulse`（unit 的 `StateDirectory=pulse` 建立；`status.json` 為 644，不含秘密） | `pulse:pulse` | 755 |

`$AGENT_ID.csr`（`/opt/pulse` 內與 `/tmp` 的副本）交出去後可以刪除。

`client.key` 與 `agent.id` 由 root 擁有、群組 pulse 只能讀（640）：即使服務帳號被入侵，也無法竄改本機身分或替換私鑰（unit 的 `ProtectSystem=strict` 另外讓 `/opt/pulse` 對服務唯讀）。Pulse 啟動時檢查這兩個檔案的權限：其他使用者有任何權限、群組可寫入，或擁有者不是 root 而群組可讀時記 `WARN`（不阻止啟動）。舊版 INSTALL 的 `pulse:pulse 600` 仍可正常運作、不會警告，建議改成新的配置（見第 10 節）。

### 6.3 多個 Kairos 節點（0.9.0）

- **有負載平衡器（TCP 透傳）或 keepalived 虛擬 IP**：`server.address` 填它的名稱即可，不需要 `fallback_addresses`。負載平衡器不可終結 TLS（8443、8444 都是 mTLS）。
- **沒有負載平衡器**：`server.address` 填第一優先的節點，`server.fallback_addresses` 依序列出其他節點（"主機" 或 "主機:埠"，IPv6 加方括號，最多 16 個）。收到 `hello_ack` 前失敗就立即改試下一個（`PLS-W2301`），全部失敗才退避（`PLS-E2302`）；連上後不主動切回第一優先。也可以用 `systemctl edit pulse` 加 `Environment=PULSE__SERVER__FALLBACK_ADDRESSES=kairos-b.example.com,10.0.20.11:8443`（逗號分隔）。
- **伺服器憑證**：所有位址都以同一個 `server.domain` 驗證，**每個節點的伺服器憑證都必須含這個名稱**（建議所有節點用同一張憑證，SAN＝服務名稱＋各節點名稱）。
- **同一個叢集**：所有位址必須屬於共用同一個資料庫的同一個 Kairos 叢集；指到另一套獨立的 Kairos 時，換節點時在途的少數 report 可能重複寫入。
- **註冊端點**：8444 不跟著備用位址走。叢集部署的安裝字串一律使用服務名稱，並把 `[enroll] address` 設為負載平衡器或虛擬 IP 的名稱（空字串時為 `server.address` 的主機）。
- **確認**：`./pulse check-config` 的「伺服器」列出備用位址；`./pulse status` 顯示「已註冊到 <位址>（Kairos 節點 <名稱>；第 N 個位址，共 M 個）」；`./pulse diag` 的 `candidates.json` 逐一測試每個位址的名稱解析與 TCP 連線。Kairos 節點優雅關機時會送排空（`drain`），Pulse 在隨機延遲後改連下一個位址，日誌記 `conn.drain PLS-I8507`，不算連線失敗。

### 6.2 本機緩衝（磁碟用量）

Pulse 0.3.0 起，取樣先寫進 `buffer/`，收到 Kairos 的確認（`ack`）才刪除；斷線、Kairos 停機維護或寫入結果不明時資料留在緩衝中，重新連上後依序補送（需要 Kairos 0.3.0 以上，見 `README.md`「本機緩衝（at-least-once）」）。相關設定（`pulse.toml` 的 `[buffer]`）：

| 設定 | 預設 | 說明 |
|---|---|---|
| `enabled` | `true` | `false` 時不寫磁碟、不補送（與 0.2.0 相同） |
| `dir` | `"buffer"` | 相對於工作目錄（`/opt/pulse/buffer`）；改到其他位置時，unit 的 `ReadWritePaths=` 也要一起改 |
| `max_mb` | `64` | 待送資料的總大小上限（MiB） |
| `max_records` | `50000` | 待送資料的筆數上限；與 `max_mb` 先到者為準，滿時丟掉最舊的資料並記 `WARN` |
| `max_age_hours` | `168` | 取樣超過 7 天的資料不再送出 |
| `replay_interval_ms` | `1200` | 補送時兩筆之間的最短間隔（每秒約 0.83 筆，低於 Kairos 的 report 速率限制每秒 2 則；1 萬台同時恢復約每秒 8,300 筆）。實際不超過 `report.interval` 的 60% |

磁碟用量最多約 `max_mb` 再加 8 MiB（預設約 72 MiB）。一筆約 0.5～3 KB，預設每 5 秒一筆時，`max_records` 約可保存 69 小時、`max_mb` 約 1.5～4 天，先到者為準。斷線 24 小時（約 17,000 筆）恢復後，以預設速率約 7.6 小時補完（期間新的取樣排在積壓之後；積壓每秒約減少 0.63 筆）。主機少、希望補得快時可把 `replay_interval_ms` 調小（500 時約 2.7 小時），大量主機同時恢復時的考量見 Kairos 的 STORAGE.md 第 11.1 節。

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
| `ProtectSystem=strict` + `ReadWritePaths=/opt/pulse/logs -/opt/pulse/buffer` | | 其餘檔案系統唯讀；因此 `agent.id` 必須在安裝時產生（第 5.1 節）。`-` 表示 `buffer/` 不存在時不影響啟動（Pulse 改為 at-most-once 並記 `ERROR`） |
| `RuntimeDirectory=pulse`＋`ExecStartPre=-+/usr/bin/install … /sys/class/dmi/id/product_serial /run/pulse/product_serial` | `/run/pulse`（0750，擁有者 `pulse`；`RuntimeDirectoryPreserve=yes`） | 0.8.0 起的主機資產需要系統序號，但 `/sys/class/dmi/id/product_serial` 只有 root 能讀（0400）。前綴 `+` 讓這一行以 root、不受沙箱限制執行，把序號複製成 `pulse` 可讀的檔案；Pulse 本身仍以 `pulse` 執行、沒有任何 capability。前綴 `-` 表示複製失敗（例如沒有 DMI 的虛擬機）不影響啟動，Pulse 不送序號並記一次 `PLS-I8009`。不想送序號時在 `pulse.toml` 設定 `[inventory] hardware_serials = false`（這幾行可保留） |

### 7.1 為什麼選擇 `StandardOutput=null`

Pulse 把日誌同時寫到 stdout 與 `logs/pulse-YYYY-MM-DD.log`。systemd 預設把 stdout 收進 journal，結果同一份日誌存兩份。本範本選擇只保留 `logs/`：

- `logs/` 依 UTC 日期分檔，並由 Pulse 依 `log.retention_days`（預設 14 天）自動清除，空間可預期；journal 的保留則取決於整台主機的 journald 設定，大量 agent 主機上不易個別控制。
- 檔案格式與 Windows 等其他平台相同，蒐集與排查的方式一致，也與 Kairos 端的 `logs/` 對得起來。
- stderr 仍進 journal（`StandardError=journal`）：設定錯誤、日誌檔無法建立、panic 這類**日誌檔還沒開好就發生**的錯誤都寫在 stderr；憑證錯誤等日誌初始化之後才發生、使 Pulse 結束的錯誤，也會另寫一行 `[ERROR] …` 到 stderr，所以啟動失敗時 `journalctl -u pulse` 仍看得到原因。

想改成只用 journal：以 `systemctl edit pulse` 加上 `[Service]`、`StandardOutput=journal`，並視需要把 `log.retention_days` 調小（`logs/` 仍會寫入，Pulse 沒有關閉檔案日誌的設定）。

0.6 重新評估後維持 `StandardOutput=null`（審計 S7）：要把日誌交給 SIEM 時，不需要經過 journal：

- **syslog**：`pulse.toml` 設定 `[log.syslog]` 的 `target`（`udp://`、`tcp://` 或 `unix:///dev/log`），以 RFC 5424 直接送出；`payload = "json"` 時 `MSG` 為一行 JSON。在背景送出，接收端無法連線時丟棄並計數，不影響服務。
- **檔案收集**：`log.format = "json"`（檔名改為 `logs/pulse-YYYY-MM-DD.jsonl`），由 rsyslog `imfile`、Filebeat、Fluent Bit 收集；規則只依 `event`、`code` 欄位判斷。
- **journald**：上述 drop-in 改 `StandardOutput=journal`；`log.stdout`（預設 `true`）會讓 stdout 與日誌檔同一格式。

### 7.2 本機指標（選用）

Prometheus 以 node_exporter 的 textfile collector 收集時（Pulse 不另外開連接埠）：

```bash
# pulse.toml
#   [metrics]
#   textfile = "/var/lib/node_exporter/textfile_collector/pulse.prom"
# 讓服務可以寫入該目錄（ProtectSystem=strict 下預設唯讀）
systemctl edit pulse
#   [Service]
#   ReadWritePaths=/var/lib/node_exporter/textfile_collector
chgrp pulse /var/lib/node_exporter/textfile_collector && chmod 775 /var/lib/node_exporter/textfile_collector
systemctl restart pulse
```

或 `listen = "127.0.0.1:9465"`（本機 HTTP，只接受 loopback）。設定後以 `pulse check-config` 確認。告警規則範例：`pulse-alerts.example.yml`。

## 8. 啟動與確認

```bash
systemctl enable --now pulse
systemctl status pulse --no-pager
```

確認清單（在 root shell 中執行；`logs/` 為 750，一般帳號讀取會 `Permission denied`；`pulse status` 一般帳號也可以執行）：

```bash
/opt/pulse/pulse --version                      # pulse <版本> (<commit>)
/opt/pulse/pulse status                         # 本機狀態：線上、結果：6 項通過（第 8.2 節）；結束代碼 0
journalctl -u pulse -n 20 --no-pager            # 只有 systemd 啟停紀錄與 stderr；沒有錯誤即正常
tail -n 30 /opt/pulse/logs/pulse-$(date -u +%F).log
```

日誌中應依序出現（有固定事件的行以「事件 代碼」開頭，沒有代碼時為 `-`，見 `ERROR-CODES.md`）：

```
[INFO] agent.start - Pulse 地端程式啟動（pulse <版本> (<commit>)，PID <PID>）...
[INFO] 工作目錄：/opt/pulse（設定中的相對路徑都以此為準）；設定檔：已讀取 /opt/pulse/pulse.toml
[INFO] 本機狀態檔：/var/lib/pulse/status.json（pulse status 讀取）
[INFO] 已載入客戶端憑證：client.pem（憑證鏈 1 張；私鑰：client.key）
[INFO] buffer.opened - 本機緩衝：/opt/pulse/buffer（待送 0 筆、0 bytes；…）
[INFO] conn.registered - 已向伺服器註冊（agent_id=…，伺服器離線判定 75000 ms，協定 v2；TLS 加密連線 monitor.example.com:8080，domain=monitor.example.com，TLSv1_3，已驗證伺服器憑證）
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
   pulse[1234]: [ERROR] auth.rejected PLS-E1105 連線中斷（連續第 1 次失敗）：伺服器回報致命錯誤 certificate_rejected：…；2.0 秒後重連
   pulse[1234]: [ERROR] tls.server_cert_rejected PLS-E1401 連線中斷（連續第 1 次失敗）：TLS 握手失敗：invalid peer certificate: UnknownIssuer（伺服器憑證不受信任（簽發者不在信任庫中）：自簽憑證或私有 CA 請以 server.ca_cert 指定）；2.0 秒後重連
   ```

   一般連線錯誤中斷滿 30 分鐘時另寫一行（`conn.outage`），例如 `[ERROR] conn.outage PLS-E2003 已中斷 30 分鐘，超過告警門檻：…`。

3. **`logs/pulse-YYYY-MM-DD.log`**：每次斷線都有一行完整紀錄。
4. **`pulse status`**（第 8.2 節）：需要人工處理的錯誤時本機狀態為「嚴重」、結束代碼 9，「連線」列寫出原因與代碼，即使之後 Kairos 剛好停機也不會被「重新連線中」蓋掉。
5. **Kairos 端**：`agents.json` 中此 agent 是否為 `"online": true`。

### 8.2 `pulse status` 與本機狀態檔

Pulse 每 10 秒（狀態改變時另外）把目前的狀態寫到 `/var/lib/pulse/status.json`（unit 的 `StateDirectory=pulse` 建立，`pulse.toml` 的 `[status] file` 可改），`pulse status` 讀取它並向 systemd 查詢 `pulse.service`，不連線、不影響服務，**一般帳號即可執行**。完整說明與 `--json` 欄位見 `README.md`「本機狀態（pulse status）」。

```bash
/opt/pulse/pulse status            # 人看的檢查列；有問題時另印原因與處理方式
/opt/pulse/pulse status --brief    # 單行，例如：online    plc-hmi-07 registered kairos.example.gov.tw:8443 cert=365d
/opt/pulse/pulse status --json     # 給監控系統（schema pulse.status/v1）
```

| 結束代碼 | 本機狀態 |
|---|---|
| 0 | 線上 |
| 8 | 警告（憑證即將到期、時鐘偏差、本機緩衝最近丟過資料等） |
| 9 | 離線（重新連線中，未滿 30 分鐘）、嚴重（需要人工處理、中斷滿 30 分鐘、服務未執行）、未知（狀態檔超過 60 秒沒有更新） |
| 10 | 沒有權限讀取狀態檔 |

- **權限**：狀態檔 `/var/lib/pulse/status.json` 為 `pulse:pulse 644`、目錄 0755，所有人可讀；執行檔 `/opt/pulse/pulse` 在 751 的 `/opt/pulse` 中，所有人可執行。狀態檔不含私鑰、token；**未啟用 mTLS 時也不含 agent_id**（此時 agent_id 等同身分憑據），一般帳號的 `pulse status` 標頭顯示「未啟用 mTLS，不顯示」，root 執行時從 `agent.id` 讀取。
- **設定檔的位置**：在任何目錄執行 `/opt/pulse/pulse status` 都會讀 `/opt/pulse/pulse.toml`（執行檔所在的目錄；一般帳號讀不到時使用預設值，狀態檔仍在預設位置）。多實例 N 請加 `--instance N`（讀 `/var/lib/pulse/N/status.json`）。
- **沿用舊 unit（沒有 `StateDirectory=`）時**：`/var/lib/pulse` 在 `ProtectSystem=strict` 下不可寫，Pulse **照常啟動與監控**，把狀態檔改寫在 `/opt/pulse/logs/status.json` 並記一次 `WARN`（`status.write_failed PLS-E8204`）；`sudo pulse status` 仍可使用，「設定」列為警告（結束代碼 8）並提示更新 unit，一般帳號則讀不到（結束代碼 10）。換上新版 unit 後，Pulse 改寫 `/var/lib/pulse` 並刪除 `logs/` 中的舊檔。
- **登入提示**（選用，預設不安裝）：在 `/etc/update-motd.d/` 加一個腳本，例如 `/etc/update-motd.d/90-pulse`（755）：

  ```sh
  #!/bin/sh
  /opt/pulse/pulse status --brief 2>/dev/null || true
  ```

  motd 由 root 執行，不需要額外權限。
- 監控系統（Nagios、Zabbix）可用 `--exit-codes=nagios`（0 OK、1 WARNING、2 CRITICAL、3 UNKNOWN，單行輸出；沒有權限讀取狀態檔時也是 3，不是 10）。

## 9. 多實例（同一台主機跑多個 Pulse）

一般一台主機只跑一個 Pulse。測試等情況需要多個實例時，每個實例必須有**自己的工作目錄、`agent.id`、客戶端憑證、`logs/` 與 `buffer/`**（共用 `agent.id` 會互相踢下線；共用 `buffer/` 會讓兩個實例互相覆寫緩衝）。

前提：第一個實例（`/opt/pulse`、`pulse.service`）已照第 4～8 節裝好。之後的實例共用使用者 `pulse` 與執行檔 `/opt/pulse/pulse`，其餘都放在 `/opt/pulse-N`（N 為 2、3…）。

> 第 5、6 節的指令都寫死 `/opt/pulse`。替其他實例做時，**所有 `/opt/pulse` 路徑都要換成 `/opt/pulse-N`**（只有執行檔 `/opt/pulse/pulse` 例外），漏改會覆寫第一個實例的 `agent.id`、私鑰或設定。下面列出完整指令，請在**同一個 root shell**（`sudo -i`）中依序執行。

```bash
N=2                                   # 每個實例換一個編號；以下所有步驟都依賴這個變數

# 1. 目錄（第 4 節）
install -d -o root  -g pulse -m 751 /opt/pulse-$N
install -d -o pulse -g pulse -m 750 /opt/pulse-$N/logs
install -d -o pulse -g pulse -m 750 /opt/pulse-$N/buffer

# 2. unit：只改 WorkingDirectory、ReadWritePaths（兩個路徑都改，所以用 g）與 StateDirectory 三行；
#    ExecStart 仍指向共用的執行檔，註解與 Documentation= 不變
sed -e "/^\(WorkingDirectory\|ReadWritePaths\)=/s#/opt/pulse#/opt/pulse-$N#g" \
    -e "s#^StateDirectory=pulse\$#StateDirectory=pulse/$N#" \
    /etc/systemd/system/pulse.service > /etc/systemd/system/pulse-$N.service
chmod 644 /etc/systemd/system/pulse-$N.service
grep -E '^(WorkingDirectory|ReadWritePaths|ExecStart|StateDirectory)=' /etc/systemd/system/pulse-$N.service
#   WorkingDirectory=/opt/pulse-2
#   ExecStart=/opt/pulse/pulse
#   StateDirectory=pulse/2          ← 狀態檔 /var/lib/pulse/2/status.json（Pulse 依 STATE_DIRECTORY 自動使用）
#   ReadWritePaths=/opt/pulse-2/logs -/opt/pulse-2/buffer
#   （RuntimeDirectory=pulse 與複製序號的 ExecStartPre= 不必改：各實例共用 /run/pulse/product_serial，
#    RuntimeDirectoryPreserve=yes 讓停止其中一個實例時不會刪除它）

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
/opt/pulse/pulse status --instance $N                        # 查詢 pulse-N.service 與 /var/lib/pulse/N/status.json
```

- 每個實例的 agent_id 不同，客戶端憑證要分別簽發；不要把第一個實例的 `client.pem`、`client.key` 複製過去（SAN 不符，啟動即結束）。
- 以 `systemctl edit pulse` 建立的 drop-in 只套用到 `pulse.service`，`pulse-N` 需要時要各自 `systemctl edit pulse-N`。
- 升級後 `pulse.service` 有變動時，重做步驟 2 重新產生 `pulse-N.service`，再 `systemctl daemon-reload`。

多個實例都從同一個 IP 連入 Kairos。Kairos 0.2.0 起啟用 mTLS 時，連線通過 hello 後改依憑證身分（agent_id）計數，同一個 IP 的實例數不受 `server.max_connections_per_ip` 限制（它只限制握手前與握手中的連線）；連舊版 Kairos 或未啟用 mTLS 時，才可能需要調高 `server.max_connections_per_ip` 或把該位址加入 `server.per_ip_allowlist`（見 Kairos 的 `README.md`「每 IP 上限、NAT 與網段允許清單」）。

## 10. 升級

**從 0.4 升級到 0.5：先升級 Kairos，再升級 Pulse**。`pulse enroll`、自動輪替（8443 feature `renew`）與過期續簽都需要 Kairos 0.5 的註冊通道，順序為：Kairos 管理者升級 Kairos → 建立簽發 CA、在 `kairos.toml` 開啟 `[enroll]`／`[pki]` → 防火牆開放 8444（只允許要註冊的主機）→ 升級 Pulse：`install -d -o pulse -g pulse -m 700 /opt/pulse/identity`、換上新版 unit（`ReadWritePaths=` 多了 `-/opt/pulse/identity`）、`systemctl daemon-reload` 後重新啟動。協定不變：先升級 Pulse 也可以，手動憑證的主機與 0.4 完全相同；只是 `pulse enroll` 要等 Kairos 開放註冊端點才能使用（之前為 `PLS-E2007`）。完整步驟見 `CHANGELOG.md` [0.5.0]「升級步驟」。

**從 0.2 升級到 0.3：建議先升級 Kairos，再升級 Pulse**（Kairos 0.3 才支援 `durable`；Pulse 升級後的第一次斷線就能補送）。先升級 Pulse 也可以：連 Kairos 0.2 時連線期間維持 at-most-once，斷線期間的積壓保留在緩衝中，Kairos 升級後才補送。完整步驟見 `CHANGELOG.md` [0.3.0]「升級步驟」。

**從 0.1.x 升級時的順序：不必同時升級，建議先升級 Pulse、再升級 Kairos。** 協定 v2 以版本協商相容 v1：新舊版本混用時自動以協定 v1 通訊（線上格式與舊版相同），Kairos 與 Pulse 可以分批、以任何順序升級，混用期間照常運作；兩端都是新版後才使用協定 v2（通用指標、主機資訊）。仍建議先升級 Pulse 的理由：

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
  pkg-new/README.md pkg-new/PROTOCOL.md pkg-new/CHANGELOG.md pkg-new/ERROR-CODES.md \
  pkg-new/THIRD-PARTY-LICENSES /opt/pulse/
# unit 有變動時：
# install -o root -g root -m 644 pkg-new/pulse.service /etc/systemd/system/pulse.service
# （多實例時，再依第 9 節步驟 2 重新產生每個 pulse-N.service）
# systemctl daemon-reload

# 4. 重新啟動並確認（第 8 節）
systemctl restart pulse          # 多實例時：systemctl restart 'pulse*.service'
/opt/pulse/pulse --version
/opt/pulse/pulse status          # 0.4 起；結束代碼 0 表示線上
tail -n 20 /opt/pulse/logs/pulse-$(date -u +%F).log
```

- `agent.id`、憑證與 `pulse.toml` 沿用，不需要重新簽發。
- 依舊版 INSTALL 安裝（`client.key`、`agent.id` 為 `pulse:pulse 600`）者，建議改成服務帳號唯讀的配置（多實例時對每個 `/opt/pulse-N` 執行相同指令）：

  ```bash
  chown root:pulse /opt/pulse/agent.id /opt/pulse/client.key
  chmod 640 /opt/pulse/agent.id /opt/pulse/client.key
  ```
- **升級到 0.8.0（指標擴充，METRICS.md）**：協定不變，與 Kairos 任意順序升級皆可；建議先升級 Kairos（行程排行與主機資產要 Kairos 0.8.0 以上、SQLite 儲存才會協商）。新版 unit 多了 `RuntimeDirectory=pulse`、`RuntimeDirectoryMode=0750`、`RuntimeDirectoryPreserve=yes` 與複製系統序號的 `ExecStartPre=-+…`，請依步驟 2 換上（多實例依第 9 節步驟 2 重新產生；這幾行各實例相同）。沒有換 unit 時服務照常執行，只是資產不含序號（記一次 `PLS-I8009`）。`buffer.max_mb` 預設由 64 改為 128（每筆緩衝記錄變大），磁碟空間有限時在 `pulse.toml` 明確設定 `[buffer] max_mb = 64`。掛載點與網路介面的上限由 32 改為 16：超過的主機第一次取樣時記一則 `INFO`（`sysinfo.snapshot`），可用 `[collect] interface_exclude` 排除不需要的介面。
- **升級到 0.4（本機狀態檔與 `pulse status`）**：協定不變，與 Kairos 0.3／0.4 任意順序升級皆可。新版 unit 多了 `StateDirectory=pulse`、`StateDirectoryMode=0755`，請依步驟 2 換上（多實例時依第 9 節步驟 2 重新產生，`StateDirectory=pulse/N`），並讓一般帳號可以執行 `pulse status`：

  ```bash
  chmod 751 /opt/pulse /opt/pulse-[0-9]* 2>/dev/null
  ```

  沒有換 unit 時服務照常啟動與監控（不會因狀態檔而失敗）：狀態檔改寫在 `/opt/pulse/logs/status.json` 並記一次 `WARN`（`PLS-E8204`），只有 root 能執行 `pulse status`，「設定」列提示更新 unit（第 8.2 節）。換上後重新啟動，`pulse status` 的「設定」列不再有 `PLS-E8204`。另外，0.4 起有固定事件的日誌行以「事件 代碼」開頭（例如 `conn.lost PLS-E2002 …`）：以行首比對日誌文字（例如 `[ERROR] 連線中斷`）的告警規則請在升級前改為比對事件或代碼，或改用包含比對（`ERROR-CODES.md`）。
- 升級前請看新版發佈包中的 `CHANGELOG.md`「破壞性變更與升級步驟」：例如設定了 `ca_cert` 時預設不再信任作業系統信任庫，伺服器憑證若由公開 CA 簽發，需在 `pulse.toml` 加上 `use_system_roots = true`；預設心跳由 1 秒改為 15 秒，Kairos 的離線判定由 5 秒變成 75 秒，需要舊行為時在 `pulse.toml` 設定 `[heartbeat] interval = 1`。
- **升級到 0.5.0**（先確認 Kairos 已是 0.5，見本節開頭）：要使用 `pulse enroll` 時先 `install -d -o pulse -g pulse -m 700 /opt/pulse/identity`，並換上新版 unit（`ReadWritePaths=` 多了 `-/opt/pulse/identity`；多實例同樣對每個 `/opt/pulse-N` 建立 `identity/` 並重新產生 `pulse-N.service`）。沿用手動憑證的部署不需要任何變更：沒有 `identity/enrollment.json` 時行為與 0.4 相同。
- **升級到 0.3.0**：新版預設啟用本機緩衝（第 6.2 節），升級前先建立緩衝目錄並換上新版 unit（`ReadWritePaths=` 多了 `-/opt/pulse/buffer`；多實例時對每個 `/opt/pulse-N` 建立目錄並重新產生 `pulse-N.service`）：

  ```bash
  install -d -o pulse -g pulse -m 750 /opt/pulse/buffer
  ```

  沒有建立目錄或沿用舊 unit 時，Pulse 照常運作，但會記 `ERROR`「無法開啟本機緩衝」並改為 at-most-once。升級後啟動日誌會有「本機緩衝：buffer（待送 0 筆…）」；連上 Kairos 0.3.0 以上時 Kairos 的上線日誌註明「durable」。回滾到 0.2.x 時 `buffer/` 中尚未送出的資料不會被送出，可以刪除。
- 新版 unit 新增 `RestartPreventExitStatus=2 3 4`（用法、設定、憑證錯誤時不再每 10 秒重試）；請依步驟 2 換上新版 unit（多實例時重新產生每個 `pulse-N.service`）。
- 從沒有 `NotifyAccess=main` 的舊版 unit 升級時，步驟 2 的 `diff` 會列出這一行；請換上新版 unit（多實例時重新產生每個 `pulse-N.service`），否則 `systemctl status` 不會顯示 `Status:` 行（見 8.1）。
- 從 0.1.x 升級時：全部 Pulse 都升級並重新註冊後，再通知 Kairos 管理者升級 Kairos。從 0.2 升級到 0.3 時則先確認 Kairos 已是 0.3。
- 回滾：`mv /opt/pulse/pulse.prev /opt/pulse/pulse && systemctl restart pulse`。
- **升級到 0.9.0（集中設定與遠端升級）**：協定不變，與 Kairos 任意順序升級皆可（兩個功能都要 Kairos 0.9.0 以上、SQLite 儲存、mTLS 才會協商）。**遠端升級需要每台主機先手動升級一次到 0.9.0**，並放好信任錨與輔助程式（第 10.1 節）；之後的版本才能由 Kairos 推送。集中設定預設啟用：大多數主機的 `pulse.toml` 只有 `[server]`，Kairos 下發的鍵就會生效；本機明確寫出的鍵優先。

### 10.1 遠端升級的準備（0.9.0，Linux）

Kairos 0.9.0 可以推送**官方簽章**的新版 Pulse（`crates/kairos-proto/REMOTE.md` 第 4 節）。Pulse 只負責下載與第一次驗證；替換、重新啟動、健康確認與回滾由 root 的 `pulse-upgrade.service` 以目前已安裝的執行檔執行，它重新驗證所有檔案、不連網路。需要：

1. **信任錨**：發佈公鑰（第 2 節的 `allowed_signers`，從另一個管道取得）放在 `/etc/kairos-pulse/allowed_signers`，root 擁有、其他人不可寫（例如 `install -d -m 755 /etc/kairos-pulse && install -o root -g root -m 644 allowed_signers /etc/kairos-pulse/allowed_signers`）。只接受 `ssh-ed25519` 金鑰與 `namespaces="kairos-pulse-release"`、`valid-after`、`valid-before` 選項（`cert-authority` 等其他選項的那一行整行拒絕）。放在其他位置時在 `pulse.toml` 設定 `[upgrade] allowed_signers = "<路徑>"`。一行安裝腳本會自動放置（A.1）。金鑰輪替：過渡期間同時列出新舊兩把（可用 `valid-before` 讓舊金鑰自動失效）；輪替只能在本機做（組態管理工具），遠端升級與集中設定都不能修改信任錨。
2. **輔助程式**：套件（A.2）會安裝並啟用。tar 安裝：

   ```bash
   install -o root -g root -m 644 pkg-new/pulse-upgrade.path pkg-new/pulse-upgrade.service /etc/systemd/system/
   systemctl daemon-reload
   systemctl enable --now pulse-upgrade.path
   ```

   `pulse-upgrade.path` 監看 `/var/lib/pulse/upgrade/request.json`（Pulse 在升級時段內寫入），檔案出現時啟動 `pulse-upgrade.service`（`Type=oneshot`、root、`RestrictAddressFamilies=AF_UNIX`，工作目錄 `/var/lib/pulse-upgrade` 為 0700）。
3. **確認**：Pulse 每次連線前重新偵測（不必重新啟動；`[upgrade]` 的設定改變時才需要重新啟動）。`pulse upgrade status` 應顯示「遠端升級：就緒」與信任錨的金鑰指紋；不是時顯示原因（`disabled`、`no_trust_anchor`、`no_helper`、`unsupported`：多實例、容器、不是 `/opt/pulse` 的 tar 安裝；`unsafe_path`〔0.10.0〕：`/opt/pulse`、`/opt/pulse/pulse`、`/var/lib/pulse-upgrade` 或它們的上層目錄不屬於 root、可由群組或其他人寫入，或 `/opt/pulse` 是符號連結——「原因」寫出是哪一個路徑；以 `stat -c '%U %a %n' / /opt /opt/pulse /opt/pulse/pulse /var /var/lib /var/lib/pulse-upgrade` 檢查，改為 root 擁有並 `chmod go-w` 後，Pulse 下一次連線時恢復為就緒。依本文件安裝的 `/opt/pulse`〔`root:pulse 751`〕符合）。

升級時：tar 安裝替換 `/opt/pulse/pulse`（舊版保留為 `pulse.prev`），`/etc/systemd/system/pulse.service` 是歷來的官方版本（沒有被修改過）時一併換新並 `daemon-reload`，改過的不動（結果的 `detail` 註明 `unit_not_updated`，服務照常執行）；deb／rpm 以 `dpkg -i --force-confold`／`rpm -U --oldpackage --replacepkgs` 安裝（`pulse.toml` 不覆寫；版本政策依實際的執行檔判斷，套件資料庫的版本不影響）。替換後在健康確認期限內（預設 600 秒）確認新版已連上 Kairos 並有 report 被確認，再穩定 60 秒才算成功；否則自動回滾。紀錄：`journalctl -u pulse-upgrade`、`/var/lib/pulse-upgrade/<upgrade_id>/upgrade.log`（`pulse diag` 收集）。回滾失敗（`PLS-E8408`）時所有暫存都保留，請依紀錄手動處理：tar 安裝 `sudo /opt/pulse/pulse upgrade rollback --yes`（換回 `pulse.prev`；可在任何目錄執行）；deb／rpm 以舊版套件 `dpkg -i`／`rpm -U --oldpackage` 重新安裝。rpm 的輔助程式需要 `cpio`（解開新版做預檢；0.9.0 的 rpm 已宣告相依）。

**從自己的 HTTPS 伺服器下載發佈檔（0.10.0，選用）**：Kairos 的版本庫可以登記為「外部 URL」（發佈檔放在客戶內部的檔案伺服器，Kairos 只保存簽章與雜湊）。Pulse 預設**不會**向 Kairos 以外的位址下載；要允許時在 `pulse.toml` 寫入允許的 URL 前綴（以目錄比對），需要時另外指定 CA，然後重新啟動 Pulse：

```toml
[upgrade]
url_allowlist = ["https://repo.example.gov.tw/kairos-pulse/"]
external_ca = "/etc/kairos-pulse/repo-ca.pem"   # 省略時用作業系統信任庫
```

下載的檔案與從 Kairos 下載的一樣，要通過本機信任錨（`allowed_signers`）的簽章與雜湊驗證才會安裝；不在白名單的 URL 一律拒絕（`pulse upgrade status`／主控台顯示 `url_not_allowed`，`PLS-W8411`）。不跟隨重新導向、不使用 HTTP 代理。

不要遠端升級：`pulse.toml` 寫入 `[upgrade] enabled = false`（或 `systemctl disable --now pulse-upgrade.path`）。不要集中設定：`[remote_config] enabled = false`。這兩個區段只能寫在 `pulse.toml`（輔助程式只讀它；環境變數只能關閉、不能開啟）。

## 11. 憑證更新與 agent_id 重新產生

以 `pulse enroll` 註冊的主機（Kairos 0.5 以上）：

- **憑證自動更新**：Kairos 支援時（8443 協定 feature `renew`），已使用壽命 2/3 或剩餘 30 天（`identity.renew_before_days`）時經既有連線送出新的 CSR，以新憑證連線成功後才取代 `identity/client.pem`／`client.key`。政策要求人工核准時記 `cert.renew_failed PLS-W1111`，核准後自動完成。
- **憑證已過期**（例如長時間關機）：Kairos 以 `CertificateExpired` 拒絕時，服務改經註冊端點（8444）以舊私鑰簽章自動續簽（過期寬限期預設 90 天，以 Kairos 為準），不需要到現場；註冊端點為「關閉」時記 `cert.grace_renew_blocked PLS-E2007`，請管理者改為「僅續簽」或「開放」。超過寬限期（`PLS-E1113`）或主機識別資訊不符（`PLS-E1114`）時需要新的 token：`./pulse enroll --force --token <新 token>`。
- **被撤銷後重新註冊**：`./pulse enroll --force --token <新 token>`（沿用 agent_id、產生新私鑰；申請會附前一張憑證的序號）。
- **複製的 VM**：`./pulse enroll --reset-identity --token <新 token> --yes`（產生新的 agent_id）。

手動簽發的憑證：

- **憑證快到期**（日誌出現「剩餘不超過 N 天」的 `WARN`）：沿用同一個 agent_id，重做 5.2（新私鑰與 CSR，私鑰同樣設為 `root:pulse 640`）請管理者簽發，替換 `client.key`、`client.pem` 後 `systemctl restart pulse`。
- **重新產生 agent_id**（例如複製 VM 後）：`systemctl stop pulse`，刪除 `/opt/pulse/agent.id`，重做第 5 節全部步驟，並請管理者撤銷舊憑證。只換 `agent.id` 不換憑證時，Pulse 啟動即因 SAN 不符而結束。

## 12. 移除

在 root shell 中執行。有多實例（第 9 節）時，**先移除所有 `pulse-N`，最後才移除 `/opt/pulse` 與使用者**：`pulse-N` 共用 `/opt/pulse/pulse` 與使用者 `pulse`，先刪掉它們會讓殘留的 `pulse-N.service` 以 `Restart=always` 每 10 秒啟動失敗一次。

以套件安裝時見第 A.2 節（先 `pulse decommission`，再 `apt purge pulse`／`dnf remove pulse`）。以下為發佈包（tar.gz）安裝的步驟。

以 `pulse enroll` 註冊的主機，在刪除 `identity/` 之前先通知 Kairos（已註冊時經 8443 除役通知；等待核准中時經 8444 撤回申請；最多 15 秒，失敗時以 `PLS-W3107`／`PLS-W3310` 警告，結束代碼 8，本機照常移除）：

```bash
systemctl stop pulse
cd /opt/pulse && ./pulse decommission --initiated-by install_script
```

```bash
# 1. 記下每個實例的 agent_id，交給 Kairos 管理者撤銷憑證（已 pulse decommission 的主機不需要）
grep -H . /opt/pulse/agent.id /opt/pulse-[0-9]*/agent.id 2>/dev/null

# 2. 停用並移除所有多實例 pulse-N（沒有多實例時不會做任何事）
for u in $(systemctl list-unit-files 'pulse-*.service' --no-legend | awk '{print $1}'); do
  systemctl disable --now "$u"
  rm -f "/etc/systemd/system/$u"
  rm -rf "/etc/systemd/system/$u.d"      # systemctl edit 建立的 drop-in
done

# 3. 停用並移除第一個實例（與遠端升級的輔助程式，0.9.0）
systemctl disable --now pulse-upgrade.path 2>/dev/null
rm -f /etc/systemd/system/pulse-upgrade.path /etc/systemd/system/pulse-upgrade.service
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
rm -rf /var/lib/pulse      # 本機狀態檔、遠端設定快取與升級暫存（StateDirectory=，停止服務後不會自動刪除）
rm -rf /var/lib/pulse-upgrade   # 遠端升級輔助程式的紀錄（0.9.0）
rm -f /etc/kairos-pulse/allowed_signers   # 遠端升級的信任錨（沒有其他產品使用時）
rm -rf /run/pulse          # 複製的系統序號（RuntimeDirectoryPreserve=yes，重新開機前不會自動刪除）
userdel pulse              # 同名群組 pulse 會一併移除

# 6. 確認沒有殘留
systemctl list-units --all 'pulse*' --no-legend      # 應無輸出
systemctl list-unit-files 'pulse*' --no-legend       # 應無輸出
ls -d /opt/pulse* /var/lib/pulse 2>/dev/null         # 應無輸出
```

請 Kairos 管理者撤銷每個實例的客戶端憑證（Kairos 的 `INSTALL.md`「撤銷與定期更新 CRL」）。Kairos 的狀態表會在離線 24 小時後自動移除這些 agent。
