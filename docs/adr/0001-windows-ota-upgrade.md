# ADR-0001：Pulse 在 Windows 上的遠端升級採用 OTA 式的版本槽機制

| 項目 | 內容 |
|---|---|
| 狀態 | 提議 |
| 日期 | 2026-11-12 |
| 決策者 | 待使用者核定 |
| 取代／修訂 | 核定後取代 [`REMOTE.md`](../../crates/kairos-proto/REMOTE.md) 第 4.4.4 節的 Windows 替換方式（就地 `msiexec`／zip 改名），並修訂第 4.3、4.4.2、12.4、18.2、18.3 節 |
| 相關 | ADR-0003（`rollback_bundle` 與回滾檔的信任，撰寫中）；`REMOTE.md` 第 4、5、17、18 節；`RELEASING.md` 第 3 節；`CHANGELOG.md`「支援期限」 |

本文中「待驗證」表示沒有在 Windows 上實際執行過、或沒有找到可引用的第一手來源；這些項目都列在第 6 節的驗證計畫或第 8 節的待決事項中，不應視為事實。

---

## 1. 脈絡

### 1.1 問題

- Linux 的遠端升級已在 0.9.0 出貨（`pulse/src/remote/upgrade/`：`UpgradeAgent` 與特權輔助程式 `pulse upgrade apply`）。
- Windows 的遠端升級在 0.10.0 評估後沒有實作（`REMOTE.md` 第 18 節）：`pulse/src/remote/install.rs` 在非 Linux 平台一律回報 `install.upgrade = "unsupported"`，Kairos 的 `kairos/src/remote/rollout.rs` 把非 Linux 主機分類為 `unsupported_platform`。
- 使用者看過評估後的決定（2026-11-12）：**「要類似 OTA 機制」**。也就是 Windows 的遠端升級要做，而且採 OTA 式的做法（版本槽、下載到非使用中的槽、原子切換、啟動後健康確認、失敗自動切回），而不是 `REMOTE.md` 第 4.4.4 節原本規劃的就地執行 `msiexec` 覆蓋。細節由本 ADR 提出並建議。

### 1.2 現況（讀程式得到的事實）

| 項目 | 現況 | 出處 |
|---|---|---|
| 服務 | 服務 `Pulse`、`ownProcess`、自動啟動、LocalSystem；命令列 `"<安裝目錄>\pulse.exe" service run`（zip 安裝另帶 `--workdir`） | `pulse/packaging/windows/pulse.wxs`、`pulse/src/winsvc.rs` |
| 失敗復原 | 10 秒後重新啟動、一天最多 3 次、非 0 結束代碼也算失敗；MSI 註冊的服務由第一次啟動時的 `ensure_failure_actions()` 補上（wixl 不支援 `MsiServiceConfigFailureActions`） | `pulse/src/winsvc.rs` |
| 檔案配置 | `C:\Program Files\Pulse\`：`pulse.exe`、文件、`pulse.toml`（`Permanent`）；**執行時的 `agent.id`、`identity\`、`buffer\`、`logs\` 也在安裝目錄**（服務的工作目錄）；只有 `status.json` 在 `%ProgramData%\Pulse\`（授予 Users 讀取） | `pulse.wxs`、`winsvc.rs`、`status_file.rs` |
| MSI | 在 Linux 容器內以 msitools 的 wixl 0.106 產生；`UpgradeCode` 固定、`ProductCode` 依版本推導、`MajorUpgrade` 不允許降版；沒有自訂動作、沒有 `ServiceControl Start`（安裝後服務是停止的） | `pulse.wxs`、`packaging/build-release-common.sh` |
| 執行檔 | `x86_64-pc-windows-gnu` 交叉編譯，**沒有 Authenticode 簽章**，建置可重現（PE 時間戳記為 0） | `RELEASING.md` 第 4 節、`CHANGELOG.md` [0.8.0] |
| 安裝方式偵測 | 執行檔在 `%ProgramFiles%\Pulse` → `msi`，否則 `zip` | `pulse/src/remote/install.rs` `windows_method` |
| ACL 檢查 | 已有 DACL 解析（`GetNamedSecurityInfoW`，只看允許項目、不展開群組），目前只用來警告 | `pulse/src/file_perms.rs` |
| CI | `windows-latest` 上跑 `cargo fmt`／`clippy`／`test` 與 `kp-e2e`（實際的 `kairos`、`pulse` 執行檔在 127.0.0.1 以 mTLS 互連）；MSI 與 zip 只在 Linux job 打包，**從來沒有在 Windows 上安裝過** | `.github/workflows/ci.yml` |
| 支援狀態 | 「Pulse 另有 Windows x86_64（0.8.0 起，**未在 Windows 實機驗證前不列入支援**）」 | `CHANGELOG.md`「支援期限」 |

### 1.3 限制

1. **沒有 Windows 實機**。目前所有 Windows 行為只經過交叉編譯與 CI 的單元／端對端測試。`REMOTE.md` 第 18.1 節的結論「沒有實機就不出貨」仍然成立；本 ADR 不推翻它，而是改變「實機」的取得方式（第 6 節）。
2. **信任邊界是 ACL**。Linux 靠「root 擁有、其他人不可寫、`O_NOFOLLOW`、`st_nlink`」；Windows 要檢查擁有者、DACL 與重新解析點（junction、符號連結），寫錯不會有錯誤訊息，結果是本機提權（`REMOTE.md` 第 18.1 節第 2 項）。
   - `%ProgramData%` 的預設 ACL 允許非特權使用者建立子目錄，特權行程在其中建立的目錄若沒有另外設定 ACL，常成為提權路徑（[CERT/CC 的說明](https://vuls.cert.org/confluence/display/Wiki/2021/06/21/Finding+Privilege+Escalation+Vulnerabilities+in+Windows+using+Process+Monitor)、[CyberArk 對防毒軟體的研究](https://www.cyberark.com/resources/threat-research/anti-virus-vulnerabilities-who-s-guarding-the-watch-tower)）。`%ProgramFiles%` 下的目錄預設繼承「只有 SYSTEM、Administrators、TrustedInstaller 可寫」的 ACL。各 Windows 版本的實際預設 ACE 需在第 6 節以 `icacls` 實測記錄（待驗證）。
   - Windows 的服務是 LocalSystem，**Pulse 本身就是特權行程**；Linux 的「非特權 Pulse ＋ root 輔助程式」的分權在 Windows 上不存在，要防的是「一般本機使用者 → LocalSystem」。
3. **MSI 產品碼與「應用程式與功能」的版本顯示**。Windows Installer 以 `ProductCode`／`ProductVersion` 記錄安裝；任何不經 `msiexec` 的升級都會讓這份紀錄與實際執行的版本不同。另外，執行檔沒有版本資源時屬於「nonversioned file」，Windows Installer 的規則是「修改時間晚於建立時間就視為使用者資料、不覆寫」（[File Versioning Rules](https://learn.microsoft.com/en-us/windows/win32/msi/file-versioning-rules)）——在 MSI 管理的目錄中就地改寫 `pulse.exe` 會讓之後的 MSI 修復／升級行為難以預測（wixl 產生的 MSI 是否帶 `MsiFileHash`、`MajorUpgrade` 的排程位置，待驗證）。
4. **服務帳號**。LocalSystem、session 0、沒有互動桌面；`msiexec /qn` 在這個環境的行為未驗證。
5. **防毒軟體鎖檔**。新寫入的執行檔會被即時掃描，期間開啟、改名、刪除可能失敗或延遲；未簽章的執行檔在某些環境會被隔離或被 WDAC／AppLocker 阻擋。
6. **重開機與斷電**。升級可能在任何一步被中斷；`MoveFileEx` 的文件沒有承諾原子性（只說明 `MOVEFILE_WRITE_THROUGH` 會等到搬移寫入磁碟，見 [MoveFileExW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw)），設計不能假設任何單一檔案操作在斷電時一定完整。
7. **打包工具**。MSI 由 wixl 產生，功能遠少於 WiX（沒有 `NeverOverwrite`、沒有失敗動作設定；自訂動作與 `RemoveFolderEx` 的支援待驗證）。方案不應依賴 wixl 沒有驗證過的功能。

---

## 2. 決策驅動因素

依優先順序：

| # | 因素 | 說明 |
|---|---|---|
| D1 | **安全** | 只安裝本機信任錨驗證過的官方發佈（`REMOTE.md` 第 4.2 節）；不引入「本機使用者 → LocalSystem」的提權路徑；不是遠端執行通道（第 5.5 節）；信任邊界盡量靠系統預設就安全的位置，而不是靠自己寫對 ACL |
| D2 | **可回滾性** | 任何一步失敗、當機、斷電之後，主機上一定有一個先前確認過的版本可以啟動，而且**回滾不需要網路、不需要 Kairos 提供任何檔案** |
| D3 | **可驗證性** | 能在沒有實機的情況下自動化驗證的比例越高越好；失敗時能分辨是哪一層（MSI、服務、升級）的問題 |
| D4 | **與 Linux 流程和協定的一致性** | 沿用 `upgrade_offer`／`upgrade_status`、`SHA256SUMS`＋`allowed_signers`、第 4.5 節的健康條件、第 4.6 節的狀態與代碼；Kairos 端的改動越少越好 |
| D5 | **維運成本** | 客戶的 SCCM／GPO 流程、MSI 安裝紀錄、磁碟用量、診斷方式；我們自己的打包與發佈流程（wixl、可重現建置） |

---

## 3. 考慮的方案

業界做法的對照（用來判斷方案是否走在有人走過的路上）：

| 產品 | 做法 | 來源 |
|---|---|---|
| Android A/B | 兩個槽；更新寫入非使用中的槽；槽有 active／bootable／successful 屬性；開機後由 `update_verifier` 標記成功，開不起來時 bootloader 自動回到舊槽 | [A/B (seamless) system updates](https://source.android.com/docs/core/ota/ab) |
| Chromium updater（Omaha 4） | 每個版本解到自己的版本目錄；新版先自我測試（qualification）再啟用（activation），啟用以鎖與「swapping」旗標保護、中斷後可接續；舊版自行移除 | [Chromium updater design doc](https://chromium.googlesource.com/chromium/src/+/main/docs/updater/design_doc.md) |
| Squirrel.Windows | `app-<版本>\` 並存目錄＋`packages\`，由 `Update.exe` 管理 | [Squirrel install process](https://github.com/Squirrel/Squirrel.Windows/blob/develop/docs/using/install-process.md) |
| Elastic Agent | 升級後進入「監看」狀態，失敗自動回到升級前的版本；回滾所需的資料在磁碟保留七天 | [Upgrade Elastic Agent](https://www.elastic.co/docs/reference/fleet/upgrade-elastic-agent)（版本目錄與指標的實作細節未查證） |
| Fleet 的 fleetd（Orbit） | Orbit 是常駐的包裝行程，以 TUF 更新並啟動 osquery；Windows 上 osqueryd 位於 `C:\Program Files\Orbit\bin\osqueryd\windows\stable\` | [Fleet：Update agents](https://fleetdm.com/docs/using-fleet/update-agents)（路徑來自社群討論，待驗證） |
| Tailscale | **就地 MSI**：下載 MSI 到 `%ProgramData%\Tailscale\MSICache`、驗證 Authenticode、把自己複製成 `tailscale-updater-*.exe` 後結束，由複本執行 `msiexec /i … /quiet /norestart`；失敗時假設是降版而先解除安裝再重試，沒有自動回到舊版 | [`clientupdate_windows.go`](https://github.com/tailscale/tailscale/blob/main/clientupdate/clientupdate_windows.go) |
| osquery、Telegraf 本體 | 沒有內建的自我更新，交給套件管理或外層工具 | osquery 見上列 Fleet 文件；Telegraf 未查證 |

以下每個方案以同一組面向描述。

### 方案 A：就地 `msiexec` 升級（`REMOTE.md` 第 4.4.4 節的原規劃，Tailscale 的做法）

- **升級步驟**：Pulse 下載並驗證 MSI → 把目前的 `pulse.exe` 複製成 `pulse-upgrader.exe`、脫離服務的工作物件啟動 → 升級程序重新驗證 → `msiexec /i 新版.msi /qn /norestart` → MajorUpgrade 停止並移除舊服務、安裝新檔案與服務 → 升級程序啟動服務 → 健康確認。
- **失敗時的狀態**：取決於 Windows Installer 交易在哪一步失敗；交易回復成功時回到舊版，回復本身失敗時可能沒有服務、沒有執行檔。1618（另一個安裝進行中）、3010（需要重開機）要另外處理。
- **斷電／當機的復原**：MSI 交易中斷電後的狀態由 Windows Installer 決定（下次 `msiexec` 會嘗試回復未完成的安裝；待驗證）。升級程序是一次性的行程，重開機後要靠服務啟動時讀 `state.json` 再把它叫起來——但此時服務可能已被 MajorUpgrade 移除。
- **回滾**：以舊版 MSI 降版安裝，需要 wxs 改為 `AllowDowngrades="yes"`，且**需要 Kairos 提供舊版 MSI**（`rollback_artifact`）——正是 `REMOTE.md` 第 17.3 節第 1 項與 ADR-0003 要處理的信任問題；回滾本身又是一次完整的 MSI 交易。
- **權限模型**：LocalSystem 寫入 `%ProgramData%\Pulse\upgrade\<id>\`（必須自己設定並檢查 ACL：只允許 SYSTEM、Administrators，路徑上沒有重新解析點），再以 LocalSystem 執行 `msiexec`（MSI 內的任何動作都以 SYSTEM 執行）。
- **與 MSI 安裝紀錄的關係**：最好——`ProductCode`、「應用程式與功能」、SCCM 偵測都與實際版本一致。
- **0.8／0.9 既有安裝的遷移**：MSI 安裝的主機手動裝一次新版 MSI 即可；zip 安裝不適用（要另做方案 B）。
- **評價**：D5 最好、D4 尚可；D2 最差（回滾依賴外部檔案與第二次 MSI 交易）、D3 最差（每個失敗模式都在 Windows Installer 內部，無法注入故障）。使用者已明確不選這條路。

### 方案 B：zip 就地替換執行檔（`REMOTE.md` 第 4.4.4 節的 zip 分支）

- **升級步驟**：停止服務 → `pulse.exe` 改名為 `pulse.exe.prev` → 寫入新版 → 啟動服務 → 健康確認。
- **失敗時的狀態**：改名之後、寫入完成之前失敗時，安裝目錄**沒有可啟動的 `pulse.exe`**；服務的 `ImagePath` 指向不存在或不完整的檔案。
- **斷電／當機的復原**：同上；要靠升級程序在重開機後接續把 `.prev` 改回來，而升級程序是一次性的行程（誰來啟動它？服務起不來）。可以用 `ReplaceFile` 把「改名＋換上」合成一步並留備份，但它的三個失敗代碼各自對應不同的殘留狀態（[ReplaceFileW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-replacefilew)），不是原子操作。
- **回滾**：`pulse.exe.prev` 改名回來；不需要外部檔案。
- **權限模型**：LocalSystem 改寫安裝目錄；暫存目錄的 ACL 問題與方案 A 相同。
- **與 MSI 安裝紀錄的關係**：對 MSI 安裝的主機最差——改寫 MSI 管理的 key path 檔案（第 1.3 節第 3 項），安裝紀錄的版本不變。因此原規劃只把它用在 zip 安裝。
- **0.8／0.9 既有安裝的遷移**：zip 安裝手動換一次執行檔。
- **評價**：實作量最小，但有一段「沒有可啟動的執行檔」的時間窗，且多數客戶用 MSI（`REMOTE.md` 第 18.4 節 (c)）。

### 方案 C：A/B 版本槽＋常駐啟動器（launcher）切換　←　建議

- **概念**：服務的進入點是一個**很少變動的啟動器**，它不做監控工作，只負責：讀「目前版本指標」、把指標指向的槽中的 `pulse.exe` 當子行程啟動並監看、在升級時切換槽、做健康確認、失敗時切回。每個版本的執行檔放在自己的槽目錄，升級只是「在旁邊建立一個新槽、改一個指標檔」，**使用中的槽與啟動器在升級過程中完全不被改寫**。
- **升級步驟**：Pulse（在使用中的槽執行）下載並驗證發佈檔 → 啟動器啟動輔助程式（使用中的槽的 `pulse.exe upgrade apply`）→ 輔助程式重新驗證、把新版解到暫時目錄、預檢、封存後改名成新槽 → 啟動器在指標檔記下「試用中」→ 停止子行程、啟動新槽 → 健康確認 → 通過則把新槽記為 `active`、舊槽記為 `previous`；不通過則清除試用紀錄、啟動原本的槽。
- **失敗時的狀態**：建槽階段失敗 → 刪除暫時目錄，舊版一直在執行，**沒有任何停機**。切換後失敗 → 指標的 `active` 從頭到尾沒有改過，啟動器啟動 `active` 即回到舊版。
- **斷電／當機的復原**：啟動器是服務本體，開機後一定會被服務控制管理員啟動；它讀指標檔：沒有試用紀錄 → 啟動 `active`；有試用紀錄 → 依試用次數與已用時間決定繼續確認或放棄。指標檔損毀時退回備份或掃描槽目錄（第 4.3 節）。新槽不完整（沒有封存標記）→ 視為不存在並清除。
- **回滾**：本機的前一槽，不需要網路、不需要 `rollback_artifact`、不經 Windows Installer。成功升級後舊槽保留為 `previous`，供管理者手動 `pulse upgrade rollback`。
- **權限模型**：所有與升級信任有關的路徑（槽、指標、暫存、信任錨）都放在**安裝根目錄**下；MSI 安裝時根目錄是 `%ProgramFiles%\Pulse`，預設 ACL 就只允許 SYSTEM／Administrators／TrustedInstaller 寫入。不在 `%ProgramData%` 放任何會被信任的檔案。ACL 檢查仍然要做（縱深防禦，zip 安裝在任意目錄時是必要條件），但 MSI 安裝的主機不靠它才安全。
- **與 MSI 安裝紀錄的關係**：MSI 降級為**開機載入器（bootstrap）**：負責安裝啟動器、註冊服務、放入初始的 `pulse.toml` 與第一個槽的內容。OTA 之後 MSI 紀錄的版本是「啟動器／初始版本」，不等於執行中的版本（第 4.9 節；這是此方案最主要的代價）。之後客戶再以 SCCM 裝更新的 MSI 仍然可行：啟動器發現自己帶的版本比 `active` 新時自動建槽並切換，比 `active` 舊時不動。
- **0.8／0.9／0.10 既有安裝的遷移**：手動（或 SCCM）安裝一次含啟動器的新版 MSI／zip；服務的 `ImagePath` 不必改（仍是 `<根>\pulse.exe service run`），啟動器第一次啟動時自動建立第一個槽。與 Linux 的「0.8 → 0.9 必須手動升級一次」（`REMOTE.md` 第 4.10 節）相同。
- **評價**：D1（預設安全的位置）、D2（回滾是「不做事」）、D3（每一步都是自己的程式，能注入故障）最好；D4 良好（狀態與訊息不變）；D5 的代價是 MSI 版本顯示與多一個啟動器要維護。

### 方案 D：獨立的更新服務（updater service）

- **概念**：另外註冊一個服務 `PulseUpdater`（LocalSystem），`Pulse` 服務本身不變（可降為低權限帳號）。更新服務接收升級請求、驗證、替換、重新啟動 `Pulse`、健康確認、回滾。相當於把 Linux 的 `pulse-upgrade.service` 做成常駐的 Windows 服務，也是 Omaha／Chromium updater 的形態。
- **升級步驟**：Pulse 寫請求 → 更新服務（常駐或由觸發啟動）重新驗證 → 以方案 A、B 或 C 的方式替換 → 以服務控制管理員重新啟動 `Pulse` → 健康確認。
- **失敗時的狀態／斷電**：取決於它用哪一種替換方式；更新服務本身開機自動啟動，可以接續未完成的升級（比方案 A、B 的一次性行程好）。
- **回滾**：同替換方式。
- **權限模型**：最清楚——可以讓 `Pulse` 以 `NT SERVICE\Pulse` 或 LocalService 執行，只有更新服務是 LocalSystem，恢復 Linux 的分權（`REMOTE.md` 第 5.1 節「`pulse` 帳號被入侵」的對策在 Windows 也成立）。但請求通道（具名管道或目錄）的 ACL 成為新的信任邊界。
- **與 MSI 安裝紀錄的關係**：MSI 要多註冊一個服務；其餘同替換方式。
- **0.8／0.9 既有安裝的遷移**：手動裝新版 MSI；zip 安裝要多執行一次服務註冊。
- **評價**：分權是真正的優點，但它**沒有回答「怎麼替換」**，只回答「誰來替換」；多一個服務就多一份安裝、啟停、失敗復原、升級自己的問題（更新服務怎麼更新？）。把 `Pulse` 降為低權限帳號本身是另一個需要實機驗證的大變更（磁碟 I/O、行程清單、SMBIOS 等蒐集在低權限下的行為，`CHANGELOG.md` [0.8.0] 已列為未驗證）。

### 方案 E：組合

| 組合 | 說明 | 評價 |
|---|---|---|
| **E1：C＋MSI 作為 bootstrap** | 方案 C，MSI／zip 只負責第一次安裝與啟動器的更新 | **建議的決策**（第 4 節） |
| E2：C＋A | 槽升級之後，背景再執行 `msiexec` 讓安裝紀錄的版本一致 | 把方案 A 的所有風險帶回來，只為了版本顯示；不建議。版本顯示改以第 8 節 U3 處理 |
| E3：D＋C | 啟動器獨立成更新服務，`Pulse` 降為低權限 | 是 E1 之後合理的演進方向（啟動器已經是「唯一需要特權的元件」）；第一版不做，設計上保留（第 4.11 節） |

### 比較總表

| 面向 | A `msiexec` | B zip 就地 | **C 槽＋啟動器** | D 更新服務 |
|---|---|---|---|---|
| 升級時的停機 | 數十秒（MSI 交易） | 數秒 | **數秒（只有切換）** | 依替換方式 |
| 有沒有「沒有可啟動版本」的時間窗 | 有（交易中） | 有（改名後） | **沒有** | 依替換方式 |
| 回滾需要外部檔案 | 需要（舊版 MSI） | 不需要 | **不需要** | 依替換方式 |
| 斷電後由誰接續 | 一次性行程（服務可能已不存在） | 一次性行程 | **服務本體（啟動器）** | 更新服務 |
| 信任邊界的位置 | `%ProgramData%`＋自訂 ACL | 同左 | **`%ProgramFiles%` 預設 ACL＋檢查** | 請求通道的 ACL |
| MSI 紀錄與實際版本 | **一致** | 不一致（且改寫 MSI 的檔案） | 不一致（不改寫 MSI 的檔案） | 依替換方式 |
| CI（無實機）可驗證的比例 | 低 | 中 | **高** | 中 |
| 新增的常駐元件 | 無 | 無 | 啟動器（同一個服務） | 多一個服務 |
| 粗估工作量 | 約 1.5 人週（原估）＋難以估計的實機除錯 | 約 1 人週 | 約 5～6 人週（第 7 節） | C 再加約 2 人週 |

---

## 4. 決策（建議）

**採用方案 E1：A/B 版本槽＋常駐啟動器；MSI／zip 只作為第一次安裝與啟動器更新的 bootstrap。不實作就地 `msiexec` 升級與 zip 就地替換。**

以下是建議的具體設計；核定後搬入 `REMOTE.md`（取代第 4.4.4 節）。

### 4.1 元件與角色

| 元件 | 位置 | 角色 | 由誰更新 |
|---|---|---|---|
| **啟動器** | `<根>\pulse.exe`（服務的 `ImagePath` 不變：`service run`） | 服務進入點；選槽、啟動並監看子行程、切換、健康確認、切回；把其他命令列（`pulse status`、`pulse enroll`…）轉送給使用中的槽 | **只由 MSI／zip（手動或 SCCM）**；OTA 不改它 |
| **代理程式** | `<根>\slots\<槽>\pulse.exe` | 原本的 Pulse：連線、回報、集中設定、下載並第一次驗證升級 | OTA |
| **輔助程式** | 使用中的槽的 `pulse.exe upgrade apply --request <固定路徑>` | 重新驗證、建槽、預檢（對應 Linux 的 `pulse-upgrade.service`，`remote/upgrade/helper.rs`） | 隨代理程式（OTA） |

- `<根>` 是安裝根目錄：MSI 為 `%ProgramFiles%\Pulse`；zip 為解開的目錄。
- 建議**第一版用同一個執行檔、以命令列決定角色**（`service run` → 啟動器；`agent run` → 代理程式；`upgrade apply` → 輔助程式）：啟動器建立第一個槽時只要複製自己，wxs 幾乎不必改（不依賴 wixl 未驗證的功能）。啟動器的程式放在獨立的模組（例如 `pulse/src/launcher/`），不使用網路、TLS、設定合併等其他模組，方便審查與日後拆成獨立的小執行檔（第 8 節 U6）。
- 啟動器刻意做得小而穩定：**簽章驗證、解壓縮、預檢都在輔助程式（隨 OTA 更新）**，啟動器只做「檢查槽的封存標記 → 改指標 → 啟動 → 監看」。這樣啟動器的錯誤面小，也不必為了修驗證邏輯而重新部署 MSI。
- 啟動器與代理程式之間的約定（命令列、指標檔與 `slot.json` 的格式、停止方式、健康訊號）以一個整數 `launcher_abi` 版本化；發佈清單（`REMOTE.md` 第 4.1.3 節）加選用欄位 `min_launcher_abi`，啟動器太舊的主機回報 `declined`（沿用 `min_upgrade_from` 的處理），需要先以 MSI 更新啟動器。

### 4.2 目錄配置

```
<根>\                                 （MSI：C:\Program Files\Pulse\）
├─ pulse.exe                          啟動器（MSI 管理的檔案；OTA 不寫）
├─ pulse.toml、agent.id、identity\、buffer\、logs\     不變：服務的工作目錄，所有槽共用
├─ allowed_signers                    信任錨（建議的新預設位置，第 4.5 節）
├─ slots\
│  ├─ current.json                    目前版本指標（第 4.3 節）
│  ├─ current.json.bak                上一次成功寫入的指標
│  ├─ 0.11.0-1a2b3c4\                 一個槽：<版本>-<commit>
│  │  ├─ pulse.exe
│  │  ├─ slot.json                    封存標記：最後才寫入；沒有它的槽視為不存在
│  │  └─ release\                     這個槽所屬發佈的 SHA256SUMS、SHA256SUMS.sig、release.json（第 4.10 節）
│  ├─ 0.11.1-5e6f7a8\
│  └─ .tmp-<upgrade_id>\              建槽中的暫時目錄（啟動器啟動時清除）
└─ upgrade\
   ├─ staging\<upgrade_id>\           代理程式下載的檔案（.part、發佈檔、SHA256SUMS、.sig、清單）
   ├─ work\<upgrade_id>\              輔助程式的私有複本與 state.json
   ├─ request.json、result.json       與 Linux 相同的交付與結果檔
   └─ health.json                     試用中的代理程式寫給啟動器的健康訊號（第 4.6 節）
```

- **為什麼是「版本命名的槽目錄、最多保留兩個」而不是固定的 `a\`、`b\`**：語意上就是 A/B（`active` 與 `previous`），但新版永遠寫進**新的目錄**，不必就地覆寫舊槽的檔案——被防毒軟體或其他行程開啟中的舊檔案只會讓「清除舊槽」延後（可重試、不影響升級），不會讓「寫入新槽」失敗。目錄名稱自我描述，診斷時一眼可見。磁碟用量：兩個槽＋升級中的暫存，約 `3 × 發佈檔大小`，與 `REMOTE.md` 第 4.6 節的暫存空間估算相同。
- **為什麼全部放在 `<根>` 而不是 `%ProgramData%\Pulse`**：第 1.3 節第 2 項。服務是 LocalSystem，寫得進 `%ProgramFiles%`；放在這裡，MSI 安裝的主機不需要自己建立任何 ACL 就只有系統管理員可寫，AppLocker 的預設規則通常也允許執行 `%ProgramFiles%` 下的檔案（待驗證）。`%ProgramData%\Pulse\status.json`（Users 可讀）維持原狀，但**啟動器不把它當成健康訊號的來源**（第 4.6 節）。
- 槽之間**共用**設定、身分、緩衝與日誌（與 Linux 相同：資料不分版本）。新版若改變本機資料格式而舊版讀不懂，以發佈清單的 `min_upgrade_from` 與「格式只增不改」的既有慣例處理；這是與 Linux 相同的殘餘風險（第 5.3 節）。

### 4.3 目前版本指標與原子切換

`slots\current.json`：

```json
{
  "schema": "kairos-pulse.slots/v1",
  "active": "0.11.0-1a2b3c4",
  "previous": "0.10.1-9f8e7d6",
  "trial": {
    "slot": "0.11.1-5e6f7a8",
    "upgrade_id": "…",
    "health_timeout_secs": 600,
    "starts": 1,
    "confirm_elapsed_secs": 0,
    "phase": "confirming"
  }
}
```

- **`active` 永遠是最後一個確認過健康的槽**。升級時不改 `active`，只新增 `trial`；確認通過才在**同一次寫入**中把 `previous ← active`、`active ← trial.slot`、`trial ← null`。因此任何時刻當機或斷電，指標不是「舊的完整狀態」就是「新的完整狀態」，而兩者的 `active` 都是可啟動的槽。這對應 Android A/B 的 successful 旗標：新槽必須由啟動後的檢查標記成功，否則退回舊槽。
- **寫入方式**：寫 `current.json.tmp` → `FlushFileBuffers` → `MoveFileExW(tmp, current.json, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`；成功後把同樣的內容以相同方式寫到 `current.json.bak`。同一個磁碟區、同一個目錄內的改名。
- **不假設改名在斷電時一定原子**（文件沒有這樣承諾，第 1.3 節第 6 項）。啟動器讀取時的容錯順序：`current.json` 可解析且 `active` 指向封存完整的槽 → 使用；否則 `current.json.bak`；否則掃描 `slots\`，選擇 `slot.json` 完整、雜湊相符、`confirmed: true` 的槽中版本最高的；都沒有 → 以啟動器自己重新建立第一個槽（第 4.8 節）。任何一次退回都記 `ERROR` 並在結果中回報。
- **不用 junction／符號連結當指標**：切換 junction 是「刪除＋建立」兩步，不是原子操作，而且重新解析點正是 ACL 檢查要拒絕的東西。**不用服務的 `ImagePath` 當指標**：`ChangeServiceConfig` 可以改，但那樣就沒有常駐的行程負責健康確認與切回，而且 MSI 修復會把它改回去。
- **槽的封存**：輔助程式把檔案寫進 `.tmp-<upgrade_id>\`，逐一 `FlushFileBuffers`，最後寫 `slot.json`（版本、commit、`pulse.exe` 的 SHA-256、`upgrade_id`、來源 `ota`／`seed`、`confirmed`），再把目錄改名為 `<版本>-<commit>`。啟動器在啟動任何槽之前都重新計算 `pulse.exe` 的 SHA-256 與 `slot.json` 比對（偵測寫到一半的檔案與事後的損毀；不是安全邊界——能改 `pulse.exe` 的人也能改 `slot.json`，安全靠目錄的 ACL）。

### 4.4 啟動器的行為

服務啟動（含開機、服務控制管理員的失敗復原重新啟動）時：

1. 檢查 `<根>` 的 ACL（第 4.5 節）；清除 `slots\.tmp-*`。
2. 讀指標（第 4.3 節的容錯）。
3. **種子（seed）**：沒有任何槽，或啟動器自己的版本**高於** `active` 且沒有進行中的試用 → 把自己複製成新槽（`source: "seed"`、`confirmed: true`），`previous ← active`、`active ← 新槽`。啟動器版本低於或等於 `active` 時不動（客戶以 SCCM 重新部署較舊的 MSI 不會造成降版）。
4. 有 `trial`：`starts` 加一並**先寫回指標**再啟動。`starts` 超過上限（建議 2）或 `confirm_elapsed_secs` 已超過 `health_timeout_secs` → 放棄試用（第 4.6 節的回滾）。否則啟動 `trial.slot` 並繼續健康確認。
5. 沒有 `trial`：啟動 `active`。
6. 以子行程啟動 `<槽>\pulse.exe agent run --workdir <根>`，放進設定了 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的工作物件（啟動器被結束時子行程一起結束，不留孤兒）。
7. 子行程結束時：一般情況下啟動器以**相同的服務特定結束代碼**結束，交給服務控制管理員既有的失敗復原（10 秒後重新啟動、一天 3 次）——保留 0.8.0 以來的行為與 `sc.exe query Pulse` 的結束代碼語意（1／3／4）。試用期間子行程結束則直接判定不健康（第 4.6 節）。
8. 收到服務的停止或關機控制：通知子行程優雅結束（建議以繼承的事件控制代碼或關閉 stdin 管道；session 0 沒有主控台，`GenerateConsoleCtrlEvent` 是否可用待驗證），最多等既有的 1.5 秒加緩衝後回報 `SERVICE_STOPPED`。

其他命令列（`pulse status`、`pulse enroll`、`pulse upgrade status`…）：啟動器把參數原樣轉送給 `active` 槽的 `pulse.exe`（繼承標準輸入輸出、傳回結束代碼）。文件中的路徑 `C:\Program Files\Pulse\pulse.exe` 因此不必改，`pulse --version` 顯示的是**執行中的版本**，另以 `pulse launcher status` 顯示啟動器版本、`launcher_abi`、兩個槽與試用狀態。

> 附帶發現：`SERVICE_FAILURE_ACTIONS_FLAG` 的文件說明「The change takes effect the next time the system is started」（[來源](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/ns-winsvc-service_failure_actions_flag)）。現行的 `ensure_failure_actions()` 是在服務第一次啟動時才設定「非 0 結束代碼也算失敗」，可能要到下次開機才生效（待驗證，列入第 6 節）。啟動器的設計不依賴這個旗標來完成切回——切回由啟動器自己決定——但一般的失敗復原仍受影響。

### 4.5 信任鏈、簽章驗證點與 ACL

**沿用 `SHA256SUMS`＋`SHA256SUMS.sig`＋本機 `allowed_signers`，規則與 `REMOTE.md` 第 4.2.2 節完全相同，不新增信任來源。**

| 驗證點 | 誰 | 驗證什麼 |
|---|---|---|
| V1 下載前 | 代理程式 | 簽章、清單與發佈檔在 `SHA256SUMS` 中的雜湊、檔名、平台、版本政策（既有的 `on_offer`） |
| V2 下載後 | 代理程式 | 實際檔案的 SHA-256（既有的 `stage`） |
| V3 建槽前 | 輔助程式 | 把 `staging\` 的檔案複製到 `work\` 後**全部重新驗證**（既有的輔助程式流程；信任錨與 `[upgrade]` 只讀 `<根>\pulse.toml`） |
| V4 建槽後 | 輔助程式 | 解出的 `pulse.exe` 預檢：`--version` 等於目標版本、`check-config --json --with-remote` 通過 |
| V5 啟動前（每次） | 啟動器 | `slot.json` 完整、`pulse.exe` 的 SHA-256 相符（完整性，不是簽章） |

- 在 Windows 上代理程式與輔助程式都是 LocalSystem，V3 不像 Linux 那樣跨越特權邊界；保留它是為了（1）與 Linux 共用同一段輔助程式的程式與測試，（2）對複本操作可避免檔案在驗證後被換掉，（3）日後把代理程式降為低權限帳號時（方案 E3）不必重新設計。
- **信任錨的位置**：建議槽式安裝的預設改為 `<根>\allowed_signers`（`REMOTE.md` 第 4.2.1 節目前寫的是 `%ProgramData%\Pulse\allowed_signers`）。仍可由 `upgrade.allowed_signers` 指定其他位置，但一律要通過下列 ACL 檢查。MSI 的 `ALLOWEDSIGNERS=<路徑>` 屬性需要自訂動作來複製檔案（wixl 是否支援待驗證）；不支援時改為安裝後執行 `pulse upgrade trust --install <檔案>`（第 8 節 U8）。
- **ACL 檢查**（輔助程式與啟動器都做；`file_perms.rs` 的 DACL 解析要從「警告」提升為「拒絕」並補上擁有者與重新解析點）。對 `<根>`、`slots\`、`upgrade\`、信任錨檔案，以及從磁碟區根目錄到 `<根>` 的每一層目錄：
  1. 擁有者是 SYSTEM、Administrators 或 TrustedInstaller。
  2. DACL 不是 NULL，且沒有任何允許項目把寫入類權限（寫入資料、附加、建立檔案／目錄、刪除、刪除子項目、寫入 DAC、變更擁有者）給上述三者以外的主體。祖先目錄只檢查「刪除子項目／改名」類權限（防止整個目錄被換掉）。
  3. 不是重新解析點（`FILE_ATTRIBUTE_REPARSE_POINT`）；開檔使用 `FILE_FLAG_OPEN_REPARSE_POINT` 後再確認，並以控制代碼取得最終路徑（`GetFinalPathNameByHandleW`）比對，避免檢查與使用之間被換掉。
  4. 一般檔案才接受（不是目錄、裝置）；硬連結數為 1。
- 不符合時 `install.upgrade` 不是 `ready`（第 4.7 節），啟動器仍照常啟動 `active`（監控不中斷），只是不接受升級。
- **是否另外要求 Authenticode**：**建議不要把 Authenticode 當成升級的信任條件**。理由：（1）信任錨應該只有一個，而且要與 Linux 一致、可離線驗證、私鑰離線保存（`RELEASING.md` 第 3 節）；Authenticode 的信任根是作業系統的憑證存放區，任何受信任的 CA 簽發的同名憑證都會通過，除非再釘選憑證，那就是第二套金鑰管理。（2）目前的執行檔沒有簽章，簽章會把簽章資料嵌進 PE，`SHA256SUMS` 必須在簽章後計算，可重現建置的比對要改為「簽章前的內容」。**但 Authenticode 對「不被防毒與應用程式控制阻擋」可能是必要的**——這是營運問題而不是信任問題，列為第 8 節 U4，並以第 6 節的 Defender／WDAC 測試結果決定。若日後簽章，啟動器可以把「Authenticode 有效且簽署者相符」當成額外的、失敗即拒絕的檢查，但通過它不足以取代 `allowed_signers`。

### 4.6 健康確認與自動切回

條件沿用 `REMOTE.md` 第 4.5.1 節與第 17.2 節第 12 項，執行者由輔助程式改為**啟動器**：

1. 子行程持續執行；試用期間**任何一次結束**（不論結束代碼）即判定不健康。
2. 健康訊號由試用中的代理程式寫到 `<根>\upgrade\health.json`（`pid`、版本、`connection.state = "registered"`、`reporting.last_ack_at`）；啟動器確認 `pid` 是自己啟動的子行程、版本等於試用槽的版本、已註冊、`last_ack_at` 晚於這次啟動。**不讀 `%ProgramData%\Pulse\status.json`**：該目錄的 ACL 不在我們的控制之內，不能讓它影響切換的決定。
3. 條件 1、2 成立後再穩定 60 秒（期間不符時重新起算）。
4. 期限 `health_timeout_secs`（120～3600，與本機設定取較大者）以**累計的確認時間** `confirm_elapsed_secs` 計算（每 10 秒寫回指標一次），不用牆上時間，避免重開機或時鐘跳動造成誤判。

- **通過**：一次寫入完成提交（第 4.3 節），寫 `result.json`（`succeeded`），清除 `staging\`、`work\`，把提交前的 `previous` 槽排入清除（保留新的 `previous`）。
- **不通過、逾時、試用啟動次數超過上限**：停止子行程 → 清除 `trial` → 啟動 `active` → 以相同條件、`max(health_timeout_secs, 300)` 的期限確認舊版恢復 → `rolled_back`（`PLS-W8407`）。試用槽保留到下一次升級或 7 天後清除（供診斷）。
- **舊版也不健康**：`rollback_failed`（`PLS-E8408`）。與 Linux 的差別：Linux 的輔助程式此後「不再嘗試任何動作」；啟動器仍是服務本體，會**繼續以一般的失敗復原規則啟動 `active`**（它是最後一個確認過的版本，也沒有更好的選擇），但不再嘗試任何升級，直到管理者處理或收到新的提議。
- 「連上 Kairos 並有一筆 report 被確認」仍是健康條件的一部分（`REMOTE.md` 第 4.5.2 節的保守選擇不變）。

### 4.7 狀態機：對應 `REMOTE.md` 第 4.6 節與差異

對 Kairos 可見的狀態（`upgrade_status.state`）與代碼**完全不變**。內部步驟的對應：

| 第 4.6 節的狀態 | Windows 槽式的行為 | 差異 |
|---|---|---|
| 閒置 → 評估 → 下載 → 驗證 → 已暫存（`accepted`／`staged`） | 相同；暫存在 `<根>\upgrade\staging\<id>\` | 只有路徑不同 |
| 交付（寫 `request.json`，`applying`） | 相同；啟動器每 5 秒檢查 `request.json`，出現時啟動輔助程式（固定的命令列，沒有來自 Kairos 或代理程式的參數） | Linux 由 `pulse-upgrade.path` 觸發；Windows 由啟動器觸發 |
| 替換中 | 拆成兩步：**建槽**（輔助程式：重新驗證、解開、預檢、封存；**舊版持續執行**）→ **切換**（啟動器：寫 `trial`、停止子行程、啟動新槽） | Linux 的替換是就地覆蓋並重新啟動；Windows 的停機只有「切換」的數秒。預檢或建槽失敗時 `failed`，沒有任何變更需要復原 |
| 確認中 | 啟動器依第 4.6 節確認 | 執行者是常駐的啟動器；重開機後由啟動器接續（Linux 由 `.path` 再次觸發輔助程式） |
| 完成（`succeeded`） | 提交指標 | 多了「提交」這個明確的原子步驟 |
| 回滾中 → `rolled_back` | 清除 `trial`、啟動 `active` | **不需要任何檔案操作**，不需要 `rollback_artifact` |
| 需人工（`rollback_failed`） | 啟動器仍持續嘗試啟動 `active` | 見第 4.6 節 |

其他差異：

- `rollback_artifact` 對槽式安裝**不適用**（與 tar 相同：「需要：無」）；Kairos 不需要為這些主機檢查版本庫中是否有目前版本的檔案，預覽中不會列為「無法自動回滾」。
- 暫存空間、`apply_window`、交付前再確認（第 17.2 節第 10 項）、同時只處理一個升級、進度回報都不變。
- 重開機接續：啟動器的指標檔取代輔助程式的 `state.json` 成為「切換之後」的唯一狀態來源；`work\<id>\state.json` 只記錄建槽階段的步驟。

### 4.8 第一次安裝、種子與既有安裝的遷移

| 情境 | 行為 |
|---|---|
| 全新安裝（MSI 或 zip） | 啟動器第一次啟動時以自己建立第一個槽（`source: "seed"`），之後與 OTA 建立的槽沒有差別 |
| 0.8／0.9／0.10 的 MSI 安裝 → 含啟動器的版本 | 安裝新版 MSI（手動、SCCM、GPO）。MajorUpgrade 換掉 `pulse.exe`，服務的命令列不變；啟動器建立第一個槽。`pulse.toml`、`agent.id`、`identity\`、`buffer\`、`logs\` 原地沿用 |
| 0.8／0.9／0.10 的 zip 安裝 | 停止服務、換上新版的 `pulse.exe`、啟動服務（服務的命令列 `service run --workdir <目錄>` 不變）。安裝目錄的 ACL 不符合第 4.5 節時 `install.upgrade` 不是 `ready`，其餘照常 |
| 之後客戶又以 SCCM 部署更新的 MSI | 啟動器版本高於 `active` → 種子建槽並切換（視為本機管理者的決定，不經健康確認的自動切回；建議仍記錄並在 `pulse launcher status` 顯示）。低於或等於 → 不動 |
| MSI 解除安裝 | 停止並移除服務、刪除 MSI 管理的檔案；`slots\`、`upgrade\` 與 `pulse.toml`、身分等一樣**留在磁碟上**（wixl 的 `RemoveFolderEx` 支援待驗證）。INSTALL 文件補上清除步驟；`pulse decommission` 可加上清除槽的選項 |

與 Linux 相同，**每台 Windows 主機都要手動升級一次**到含啟動器的版本，之後才能從 Kairos 推送（`REMOTE.md` 第 4.10 節）。

### 4.9 與 MSI 安裝紀錄的關係

- OTA 之後，「應用程式與功能」與 `ProductCode` 顯示的是**最後一次以 MSI 安裝的版本**（啟動器的版本），不是執行中的版本。執行中的版本以 Kairos 主控台、`pulse --version`、`pulse status` 為準。
- 這會改變 `REMOTE.md` 第 18.3 節第 2 項的通過條件（原本要求三者一致）與第 12 項（SCCM 偵測）。以「產品碼存在」或「`pulse.exe` 存在」為偵測規則的 SCCM 部署不受影響；以「版本 ≥ X」為規則的部署會判定為需要更新並重新部署 MSI——這是安全的（第 4.8 節：啟動器版本較新時才切換）。
- 是否讓啟動器在提交後改寫解除安裝登錄機碼的 `DisplayVersion` 讓顯示一致，列為第 8 節 U3；建議第一版不做（它只改顯示，Windows Installer 自己的資料庫不變，而且 MSI 修復可能把它改回去——待驗證）。

### 4.10 與協定的關係

- **協定版本不升、不新增 feature**。`upgrade` feature 的訊息（`upgrade_get`／`upgrade_offer`／`upgrade_chunk*`／`upgrade_status`）完全沿用。
- **新的安裝方式值 `install.method = "slot"`**：
  - `REMOTE.md` 第 2.1 節已規定 `method` 是 1～32 bytes 的小寫字串、不認得的值由 Kairos 原樣保存並顯示為「其他」；`release::artifact_kind` 對不認得的值回 `None`，推送中標為 `skipped`（`unsupported_platform`）。因此 0.9／0.10 的 Kairos 遇到 `slot` 的主機**不會提議升級**，行為安全。
  - 新版 Kairos：`slot`＋Windows x86_64 → 發佈檔 `pulse-<版本>-<commit>-windows-x86_64.zip`（既有的格式，內含 `pulse.exe`）；移除 `rollout.rs` 中「非 Linux 一律 `unsupported_platform`」的判斷，改為只接受 `slot`。不送 `rollback_artifact`。
  - 為什麼不沿用 `zip`：`zip`／`msi` 在 0.9／0.10 的語意是「舊式配置、不支援遠端升級」，沿用會讓 Kairos 無法分辨主機是否有啟動器。`msi`、`zip` 兩個值保留給舊式配置，永遠是 `unsupported`／`no_helper`。
  - 名稱用 `slot` 而不是 `ota` 或 `windows_slot`：它描述的是配置方式，日後 Linux 的 tar 安裝若要採用同樣的配置可以共用。
- **`install.upgrade` 的值**：

  | 情況 | 值 |
  |---|---|
  | 槽式配置、信任錨與 ACL 都符合、`upgrade.enabled` | `ready` |
  | 舊式配置（沒有啟動器） | `no_helper`（沿用既有的值：語意就是「缺少輔助程式」，主控台已有對應的說明） |
  | `<根>` 或其祖先的 ACL 不符 | 建議新增 `insecure_path`（舊版 Kairos 顯示為「其他」、分類為 `not_ready`）；不新增值的替代做法是 `unsupported` 並在本機 `pulse upgrade status` 說明原因 |
  | 信任錨不存在或 ACL 不符 | `no_trust_anchor` |

- **hello 的 `install` 物件加選用欄位** `launcher`（啟動器版本）與 `launcher_abi`；`REMOTE.md` 第 2.1 節已規定「其他欄位忽略」。主控台可據此顯示「啟動器需要以 MSI 更新」。
- **發佈清單**加選用欄位 `min_launcher_abi`（清單的解析目前沒有 `deny_unknown_fields`，舊版忽略；實作時以測試向量確認）。
- **代碼**：新的情況需要的代碼（啟動器無法讀取指標而退回備份、種子建槽、ACL 不符而拒絕升級、啟動器版本過舊）依慣例由設計包正式分配；本 ADR 不暫定號碼。

### 4.11 與回滾檔信任規則（ADR-0003）的關係

`REMOTE.md` 第 17.3 節第 1 項確立的原則是：**回滾目標必須是本機自己驗證過的東西，不能是 Kairos 在回滾當下提供的東西**（否則被入侵的 Kairos 可以故意讓健康確認失敗，藉回滾安裝任意內容）。A/B 槽天然符合這個原則，本 ADR 讓「前一槽」能直接套用 ADR-0003 的規則：

- 每個由 OTA 建立的槽都在 `slots\<槽>\release\` 保存**它所屬發佈的** `SHA256SUMS`、`SHA256SUMS.sig` 與清單（V3 驗證通過的那一份），`slot.json` 記錄 `pulse.exe` 的 SHA-256。這與 Linux 的 `<狀態目錄>/upgrade/releases/<版本>/` 是同一種資料，建議 ADR-0003 把兩者定義成同一個「已驗證發佈的本機紀錄」。
- 切回前一槽是本機動作：不下載、不接受 `rollback_artifact`、不接受 `rollback_bundle`。**槽式安裝的提議即使帶了這兩個欄位也一律忽略。**
- 自動切回屬於 `REMOTE.md` 第 4.8 節的「自動回滾」，不受降版政策限制。

**需要 ADR-0003 回答的問題**（本 ADR 的建議附在後面，以 ADR-0003 的結論為準）：

| # | 問題 | 本 ADR 的建議 |
|---|---|---|
| Q1 | 回滾目標的信任是「建立當時驗證過」就夠，還是回滾當下要以**目前的**信任錨重新驗證簽章？（金鑰在這之間被移除或 `valid-before` 到期時，前一槽還能不能用？） | 回滾是安全動作，不應因金鑰輪替而失效：回滾當下只要求完整性（V5），簽章重新驗證的結果只記錄、不阻擋。若 ADR-0003 要求重新驗證，啟動器必須內含簽章驗證，啟動器就不再是「很少變動的小元件」，第 4.1 節要跟著調整 |
| Q2 | **種子槽**（由 MSI／zip 安裝、沒有經過 `allowed_signers` 驗證）可不可以當回滾目標？ | 可以。它是本機管理者安裝並正在執行的版本，等同 Linux tar 安裝的 `pulse.prev`（同樣沒有簽章驗證）。請 ADR-0003 把「本機管理者安裝、且曾經健康執行的版本」明列為合法的回滾來源 |
| Q3 | 「已驗證發佈的本機紀錄」的格式與位置是否統一（Linux 的 `upgrade/releases/<版本>/` 與 Windows 的 `slots\<槽>\release\`）？紀錄的鍵是 (版本, commit) 還是發佈檔的 SHA-256？ | 統一格式；鍵用 (版本, commit, 發佈檔 SHA-256) |
| Q4 | `rollback_bundle` 是否對所有安裝方式都送？ | 只對 deb／rpm 送（它們的回滾需要套件檔）。Kairos 對 `slot` 與 `tar` 不送；Pulse 收到也忽略 |
| Q5 | 是否允許 Kairos 遠端要求「切回前一槽」（不下載的快速降版）？ | 第一版不提供。若要做，它是「遠端降版」，必須同時符合第 4.8 節的兩個開關，並且是新的白名單動作（第 5.5 節），不能藏在 `upgrade_offer` 裡 |
| Q6 | 提交之後 `previous` 槽保留多久？保留期間的手動回滾（`pulse upgrade rollback --yes`）是否受版本政策限制？ | 保留到下一次升級成功為止（最多兩個槽）；本機管理者的手動回滾不受限制（與 tar 相同） |

### 4.12 啟動器自身的升級

- **第一版：啟動器只由 MSI／zip 更新**（客戶既有的 SCCM／GPO／手動流程）。OTA 不改寫 `<根>\pulse.exe`，因此不碰 MSI 管理的檔案（第 1.3 節第 3 項的問題不會發生），也沒有「更新程式更新自己失敗」這個最難回復的情況。
- 靠三件事讓啟動器很少需要更新：邏輯小（第 4.1 節）、`launcher_abi` 版本化、驗證與預檢都在可 OTA 的輔助程式。
- 啟動器有安全修正而必須更新時：發佈清單提高 `min_launcher_abi`（或另加 `min_launcher`），主控台列出需要以 MSI 更新的主機。
- 保留的演進路徑（不在第一版）：（a）啟動器也做成槽——需要一個更小的、永不更新的第零層，或改寫服務的 `ImagePath`，複雜度高；（b）方案 E3：啟動器獨立成更新服務。兩者都等第一版在實機上運作一段時間後再評估。

---

## 5. 後果

### 5.1 正面

- **回滾變成「不做事」**：`active` 在確認通過前不會被改動；斷電、當機、升級中重開機之後，服務本體（啟動器）一定會啟動最後確認過的版本。不存在「沒有可啟動版本」的時間窗。
- **不需要 `rollback_artifact`**：Windows 完全避開 `REMOTE.md` 第 17.3 節第 1 項的信任問題與「套件資料庫與執行檔不一致」（第 17.5 節第 3 項）。
- **不經 Windows Installer**：1618、3010、交易回復、`AllowDowngrades`、session 0 的 `msiexec` 都不在升級路徑上；`REMOTE.md` 第 18.3 節的 12 項中有 2 項不再適用、3 項大幅簡化（第 6.3 節）。
- **信任邊界落在系統預設就安全的位置**（MSI 安裝時），自訂 ACL 的程式從「唯一防線」降為縱深防禦。
- **可驗證性高**：每一步都是自己的程式與檔案操作，可以在 CI 的 Windows runner 上實際執行並在每一步之間注入故障（第 6 節）。
- **升級的停機縮短**為切換的數秒；建槽與預檢在舊版執行中完成。
- 協定與 Kairos 端的改動很小（一個新的 `install.method` 值與發佈檔對應）。

### 5.2 負面

- **MSI 安裝紀錄的版本與執行中的版本不一致**；需要文件、主控台與 SCCM 指引說明（第 4.9 節）。
- **多一個常駐元件**：啟動器的錯誤會影響所有槽，而且只能以 MSI 修正。行程樹變成兩層（啟動器＋代理程式），`pulse status`、`pulse diag`、文件與支援流程都要跟著改。
- **工作量明顯高於原估**：約 5～6 人週（原 P3 估 1 人週＋W0 0.5 人週），見第 7 節。
- **所有既有的 Windows 安裝都要手動升級一次**，而且這一次改變了檔案配置（多了 `slots\`、`upgrade\`）。
- 磁碟用量增加（兩個槽＋暫存；以目前的執行檔大小估計為數十 MB）。
- 每次升級都會有一個**新路徑的、未簽章的執行檔**第一次被執行，比就地覆蓋更容易觸發防毒軟體的啟發式判斷（待驗證；第 8 節 U4）。
- `REMOTE.md` 第 4.3、4.4.2、4.4.4、12.4、18 節與 INSTALL、設計包的 SCCM 文件要修訂。

### 5.3 殘餘風險

| 風險 | 說明 | 緩解 |
|---|---|---|
| 啟動器的缺陷 | 選槽或切回的邏輯錯誤會讓服務起不來，而且 OTA 修不了 | 邏輯極小、以性質測試與故障注入涵蓋（第 6 節）；`launcher_abi`；支援階段前至少經過兩次真實發佈的升級 |
| 共用資料的格式 | 新版寫了舊版讀不懂的 `buffer\`、快取或狀態檔，切回後舊版無法啟動 → `rollback_failed` | 與 Linux 相同的風險；格式只增不改、`min_upgrade_from`；CI 的回滾情境以「新版先執行一段時間再切回」驗證 |
| ACL 檢查的漏洞 | 檢查邏輯判斷錯誤時，zip 安裝在寬鬆目錄的主機可能有提權路徑 | MSI 安裝不依賴它；CI 以實際的非特權帳號攻擊（第 6 節）；VM 再以真實的標準使用者與網域帳號重做 |
| NTFS 以外的檔案系統、非本機磁碟 | 改名與排清的行為不同 | `<根>` 不在本機固定磁碟（`GetDriveTypeW`）時 `unsupported`；ReFS 待驗證 |
| 防毒軟體隔離新槽 | 建槽後 `pulse.exe` 被刪除或鎖住 | V5 在切換前檢查，失敗為可診斷的 `failed`，舊版不受影響；切換後被隔離等同啟動失敗，自動切回 |
| 健康條件依賴 Kairos | 切換期間 Kairos 停機會造成不必要的切回 | 既有的保守選擇（`REMOTE.md` 第 4.5.2 節），切回成本在槽式下很低 |
| 沒有涵蓋的環境 | 網域原則（GPO 的軟體限制、WDAC、AppLocker）、Server Core、非英文系統、Windows Server 2016 以前 | 支援矩陣只列驗證過的版本（第 7 節的出貨條件） |
| 啟動器與代理程式同為 LocalSystem | 代理程式被入侵等於 SYSTEM 被入侵（現況即如此，本 ADR 沒有讓它變差，也沒有改善） | 方案 E3 保留為後續 |

---

## 6. 驗證計畫

原則不變：**沒有在 Windows 上實際執行過的行為不出貨。** 改變的是取得「Windows 上實際執行」的方式——分三層，前兩層不需要使用者先購買任何設備。

### 6.1 第一層：GitHub Actions 的 Windows runner（立刻可以開始）

依 GitHub 的文件，GitHub 代管的 Windows runner「configured to run as administrators with User Account Control (UAC) disabled」，目前提供 `windows-2022`、`windows-2025`（`windows-latest` 對應 2025）與 arm64 的 `windows-11-arm`（[GitHub-hosted runners](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)）。也就是說，CI 可以**真的**安裝 MSI、註冊並啟停服務、以 LocalSystem 在 session 0 執行 Pulse、建立本機帳號、修改 ACL。現有的 `kp-e2e` 已經在 `windows-latest` 上啟動實際的 `kairos` 與 `pulse` 執行檔，可以直接沿用。

新增 job `windows-system`（`needs: package`；矩陣 `windows-2022`、`windows-2025`），下載 `package` job 產出的 MSI 與 zip（**也就是實際要發佈的、由 wixl 交叉產生的檔案**），以測試金鑰簽署 `SHA256SUMS`（沿用 `remote/upgrade/testsign.rs` 與 `tests/systemd/` 的四個建置：正常、啟動即結束、正常、啟動即結束），腳本放在 `tests/windows/`（PowerShell，對應 `tests/systemd/run.sh`）：

| 群組 | 情境 |
|---|---|
| S0 基礎（補 0.8.0 的未驗證項目） | `msiexec /i … /qn` 安裝、服務註冊與命令列、`pulse enroll` 後啟動、工作目錄、`status.json`、停止與重新啟動、MajorUpgrade 到下一版、`msiexec /x` 移除後留下的檔案、zip＋`pulse service install --start`、各目錄的實際 ACL（`icacls` 輸出存成 artifact） |
| S1 槽與啟動器 | 種子建槽、`pulse --version` 的轉送、服務停止時子行程一起結束（工作物件）、子行程結束代碼 → 服務結束代碼 → 失敗復原 |
| S2 升級成功 | 以實際的 Kairos 推送 `test.1`：建槽、切換、健康確認、提交、`succeeded`、暫存清除、`previous` 保留 |
| S3 自動切回 | 推送「啟動即結束」的 `test.2`：切回、`rolled_back`、推送自動暫停；再推送 `test.3` 成功 |
| S4 故障注入 | 啟動器與輔助程式在每個步驟之後留有測試用的中斷點（環境變數指定，只在測試建置生效）：在每一點 `taskkill /F` 整個行程樹後重新啟動服務，斷言最後一定收斂到「`active` 在執行、狀態正確回報」。另外直接破壞檔案：截斷 `current.json`、刪除 `current.json`、刪除 `slot.json`、改壞槽內的 `pulse.exe` |
| S5 信任與 ACL | 不信任的金鑰（`PLS-E8401`）；以 `net user` 建立**不屬於 Administrators 的本機帳號**，以該帳號的身分（排程工作）嘗試：在 `<根>\upgrade\`、`slots\` 建立或替換檔案、建立 junction、預先建立 `%ProgramData%\Pulse` 並放入偽造的 `status.json`；把信任錨或 `<根>` 的 ACL 放寬後確認 `install.upgrade` 不是 `ready`；zip 裝在 `C:\pulse-test`（繼承磁碟根目錄的 ACL）時的判斷 |
| S6 鎖檔 | 另一個行程以不共用的方式開啟舊槽的 `pulse.exe`、`staging\` 的檔案、`current.json`，確認重試與可診斷的錯誤，不會卡住 |
| S7 與 MSI 並用 | 試用期間安裝更新的 MSI；OTA 之後安裝較舊的 MSI（不應降版）；OTA 之後安裝較新的 MSI（種子切換）；OTA 之後 `msiexec /fa` 修復 |

限制（必須誠實列出）：runner 不能重開機（job 會結束）、沒有真正的斷電、是 Server 版而不是用戶端版、沒有加入網域、每次都是全新的映像（沒有長時間執行、沒有既有的 0.8 安裝歷史）、Microsoft Defender 即時保護在 runner 映像上是否開啟**未查證**（腳本以 `Get-MpComputerStatus` 記錄；開啟時 S2／S3 的結果可當作第 10 項的初步證據，但不能取代 VM）。故障注入以「強制結束行程」近似當機，不能證明檔案系統在斷電時的行為。

### 6.2 第二層：自管的 Windows VM（不需要購買設備）

使用者既有的 PVE（kp-build 所在）上建立 Windows VM，使用 Microsoft 提供的**評估版**（Windows Server 2022／2025 Evaluation、Windows 11 Enterprise Evaluation；授權條款與有效天數以 Microsoft 下載頁為準，待確認）。把 VM 註冊成 GitHub Actions 的 self-hosted runner 或以 `kp-remote.sh` 的方式遠端執行同一套 `tests/windows/` 腳本，另外加上第一層做不到的項目：

| 項目 | 做法 |
|---|---|
| 正常重開機 | 在第 4.7 節的每個狀態（建槽中、已寫 `trial` 未啟動、確認中、切回中）重開機，開機後斷言收斂 |
| 斷電 | PVE 的 `qm stop`（直接切斷，不經客體作業系統關機）在上述每個狀態各做多次（建議每點 20 次），開機後斷言：服務啟動、`active` 可執行、指標可解析或已退回備份。這是唯一能驗證第 4.3 節「不假設改名原子」的容錯是否足夠的測試 |
| Defender 即時保護、雲端保護開啟 | 新槽是否被隔離、掃描造成的延遲與鎖檔 |
| 用戶端版 Windows | Windows 11（23H2／24H2 以上） |
| Windows Server 2019 | GitHub 已不提供 2019 的 runner，只能在 VM 驗證（若要列入支援） |
| 快照還原 | 每個情境從乾淨的快照開始；另保留一個「0.8.0 MSI 安裝並執行一段時間」的快照驗證遷移 |
| 長時間執行 | 連續 20 次以上的升級／切回循環，檢查槽的清除與磁碟用量 |

### 6.3 第三層：仍需真實環境或客戶試點

網域加入與 GPO（軟體限制原則、AppLocker、WDAC）、SCCM 的部署與偵測規則、第三方防毒／EDR、Server Core、非英文語系、實體機的斷電（寫入快取的行為與 VM 不同）。這些列為「支援」階段的條件或明列為未驗證（第 7 節）。

### 6.4 `REMOTE.md` 第 18.3 節 12 項清單的對應

| # | 原情境 | 在槽式設計下 | CI 可驗 | 需 VM | 備註 |
|---|---|---|:-:|:-:|---|
| 1 | MSI 安裝、放好 `allowed_signers` | 保留；hello 的 `install` 改為 `slot`／`ready`；信任錨位置依第 4.5 節 | ✓（S0、S1） | 用戶端版再做一次 | `ALLOWEDSIGNERS=` 屬性視 wixl 的支援而定（U8） |
| 2 | 推送成功 | 保留，但通過條件改寫：沒有 `msiexec`、沒有「脫離工作物件」；改為「建槽期間舊版持續回報、切換後新槽為 `RUNNING`、提交後 `previous` 保留、暫存清除」。**「應用程式與功能的版本一致」這一條移除**（第 4.9 節） | ✓（S2） | 用戶端版再做一次 | |
| 3 | 啟動即結束 → 回滾 | 保留；回滾方式改為切回前一槽，不需要 `rollback_artifact` 與 `AllowDowngrades` | ✓（S3） | | |
| 4 | zip 安裝的升級與回滾、鎖檔 | 與 MSI 走同一條槽式路徑；`pulse.exe.prev` 不再存在 | ✓（S2、S3、S6 以開啟控制代碼模擬） | 真實防毒的鎖檔 | |
| 5 | 確認期間重開機（正常關機、斷電） | 保留並擴大到每個狀態 | 部分（S4：強制結束行程後重新啟動服務，只是近似） | **✓ 必須**（6.2） | 這一項決定能否進入預覽 |
| 6 | `msiexec` 1618 | **不再適用**（升級不經 Windows Installer）。改為「OTA 進行中同時安裝 MSI」 | ✓（S7） | | |
| 7 | `msiexec` 3010 | **不再適用** | — | — | 槽式升級不需要重開機 |
| 8 | ACL 與提權 | 保留並擴大（重新解析點、祖先目錄、`%ProgramData%` 搶先建立、偽造健康訊號） | ✓（S5，以真正的非系統管理員帳號執行） | ✓ 以互動登入的標準使用者與網域使用者重做 | runner 的預設帳號是系統管理員且 UAC 關閉，測試**必須**另建帳號 |
| 9 | 簽章錯誤 | 保留，不變 | ✓（S5） | | 與平台無關的部分已有單元測試 |
| 10 | 防毒軟體 | 保留；對象改為新槽的 `pulse.exe` 與 zip（不再有 `pulse-upgrader.exe` 與 MSI） | 僅在 runner 的 Defender 開啟時當作初步證據 | **✓ 必須** | 未簽章執行檔的行為是 U4 的依據 |
| 11 | 服務帳號與 session 0 | 升級不再執行 `msiexec`；剩下「LocalSystem、session 0 下啟動器能啟動子行程、優雅停止子行程」 | ✓（S1；runner 上的服務就在 session 0） | 由 GPO／SCCM 在無人登入時部署 MSI 的情況 | |
| 12 | SCCM 並用（選做） | 保留；依第 4.9 節改寫預期（版本規則會觸發重新部署，屬預期行為） | 以腳本模擬偵測規則（產品碼、檔案存在、版本） | 真正的 SCCM 屬第三層 | |

新增、原清單沒有的項目：指標檔損毀的容錯（S4）、種子與較新／較舊 MSI 的互動（S7）、啟動器版本過舊時的 `declined`、連續多次升級後的槽清除（6.2）、`SERVICE_FAILURE_ACTIONS_FLAG` 何時生效（第 4.4 節的附帶發現）。

---

## 7. 實作計畫

工作量為粗估（一人、熟悉現有程式）；最大的不確定性在第一次於真實 Windows 上執行時會發現多少 0.8.0 遺留的問題（階段 0）。

| 階段 | 內容 | 粗估 | 完成條件 | 對外狀態 |
|---|---|---|---|---|
| **0 基礎驗證** | CI 新增 `windows-system` job 與 `tests/windows/` 的 S0：在 runner 上實際安裝現有的 MSI／zip、啟停服務、MajorUpgrade、移除；修正發現的問題；記錄各目錄的預設 ACL。**不含任何升級行為** | 0.5～1 人週 | S0 在 `windows-2022`、`windows-2025` 連續通過；`CHANGELOG.md` [0.8.0]「尚未實機驗證」中 CI 能涵蓋的項目劃掉 | 不支援（不變） |
| **1 啟動器與槽** | `pulse/src/launcher/`：指標、種子、子行程監看、工作物件、命令列轉送、`pulse launcher status`；`install.method = "slot"`；`install.upgrade` 仍是 `unsupported`。S1、S4 中與升級無關的部分 | 1.5 人週 | S0、S1 通過；從 0.10 的 MSI／zip 升級到這一版的遷移通過；啟動器的指標容錯有性質測試 | 不支援；**可先隨一個版本出貨**（配置改變但沒有新的特權行為），讓之後的升級有基礎 |
| **2 升級** | 輔助程式的 Windows 版（`staging`／`work`、ACL 檢查提升為拒絕、zip 解開、預檢、建槽封存）；啟動器的試用、健康確認、提交、切回、重新啟動後接續；`health.json`；Kairos 的 `slot` → zip 對應與資格判斷；發佈清單 `min_launcher_abi` | 1.5～2 人週 | S2、S3、S5、S6、S7 通過；`install.upgrade` 在測試建置中為 `ready`，**發佈建置仍為 `unsupported`**（以編譯期開關隔開，不是設定檔開關——回應 `REMOTE.md` 第 18.1 節第 2 項） | 不支援 |
| **3 故障注入** | S4 的完整矩陣；CI 夜間執行；修正 | 0.5～1 人週 | S4 在兩個 runner 映像上連續 20 次全數通過 | 不支援 |
| **4 VM 驗證** | 第 6.2 節：PVE 上的 Windows Server 2022、Windows 11（選做 2019、2025）；重開機、斷電、Defender、長時間循環；結果記入 CHANGELOG | 1 人週＋等待時間 | 第 6.4 節中「需 VM」的項目在 Windows Server 2022 與 Windows 11 全數通過；斷電測試每個狀態 20 次沒有任何一次需要人工處理 | **→ 預覽**：發佈建置開啟；Windows 預設 `upgrade.enabled = false`，需明確開啟（U5）；CHANGELOG 列出驗證過的版本與未驗證的環境 |
| **5 支援** | 至少兩個真實的發佈經 OTA 在自管 VM 與（若有）客戶試點主機升級，沒有 `rollback_failed`；第三層中選定的項目（至少一種 GPO／AppLocker 組態）；U3、U4 定案；INSTALL、SCCM 文件、主控台說明完成 | 0.5 人週＋試點期間 | 上述條件；`CHANGELOG.md`「支援期限」的平台列移除「未在 Windows 實機驗證前不列入支援」並改列驗證過的版本 | **→ 支援**；預設值是否與 Linux 一致另行決定 |

合計約 5.5～7 人週（原方案 A 的估計為 1.5 人週，但不含它無法估計的實機除錯）。階段 0 與 1 不依賴任何待決事項，可以立刻開始。

需要同步修改的文件（核定後）：`REMOTE.md` 第 2.1、2.4.1、4.3、4.4.2、4.4.4、4.5、4.6、4.10、12.4、18 節；`PROTOCOL.md`「遠端管理」的 `install.method`；`pulse/packaging/INSTALL.md` A.3 與第 10.1 節；`pulse/README.md`；`RELEASING.md`（Windows 的系統測試成為發佈前檢查）；`CHANGELOG.md`；設計包 `pulse/sccm-deployment.md`、`pulse/cli-spec.md`（`pulse launcher status`、兩層行程）。

---

## 8. 待決事項（需要使用者決定）

| # | 事項 | 選項 | 建議 |
|---|---|---|---|
| U1 | 是否核定方案 E1（槽＋啟動器，MSI 為 bootstrap），並放棄就地 `msiexec` 與 zip 就地替換 | 核定／改採其他方案 | **核定**。它最符合「類似 OTA」的決定，回滾與可驗證性最好 |
| U2 | 驗證環境 | (a) 先用 GitHub Actions runner（第一層），再於既有的 PVE 建立評估版 Windows VM（第二層）；(b) 購買或租用實體機；(c) 只用 CI | **(a)**。不需先採購；斷電與 Defender 一定要 VM，(c) 不足以進入預覽 |
| U3 | 「應用程式與功能」的版本顯示 | (a) 接受不一致，以文件與主控台說明；(b) 啟動器提交後改寫 `DisplayVersion`；(c) 背景再跑 `msiexec`（方案 E2） | **(a)**，並在階段 4 的 VM 上實驗 (b) 是否穩定後再決定；不建議 (c) |
| U4 | Authenticode 程式碼簽章 | (a) 不簽；(b) 取得憑證並簽章，但不作為升級的信任條件；(c) 簽章並作為額外的必要檢查 | 信任條件維持 `allowed_signers`（不選 c）。是否 (b) 依階段 4 的 Defender／AppLocker 測試結果決定；若客戶環境使用 WDAC，(b) 很可能是進入「支援」的必要條件。注意 (b) 需要調整可重現建置的比對與 `SHA256SUMS` 的產生順序，並有憑證費用與保管問題 |
| U5 | 預覽期間 Windows 的預設值 | (a) `upgrade.enabled` 預設 `false`，需明確開啟；(b) 與 Linux 相同預設 `true` | **(a)**；進入「支援」時再評估是否改為 (b) |
| U6 | 啟動器的形態 | (a) 同一個 `pulse.exe`、以命令列分角色；(b) 獨立的小執行檔 `pulse-launcher.exe`，MSI 另帶完整的代理程式作為種子 | **(a)** 作為第一版（wxs 幾乎不改、種子只要複製自己）；模組邊界依 (b) 的方式切好，日後可拆 |
| U7 | 槽的內容來源與解壓縮 | (a) 沿用既有的 `windows-x86_64.zip`，新增 zip 解壓縮的依賴（需通過 `cargo deny`）；(b) 呼叫系統的 `tar.exe`（Windows 10 1803／Server 2019 起內建，待驗證）；(c) 另外發佈未壓縮的 `pulse.exe` 或 `.tar.gz` | **(a)**：不新增發佈檔種類、不依賴系統工具的版本。若不願新增依賴則 (c) |
| U8 | 信任錨的預設位置與交付方式 | (a) `<根>\allowed_signers`，安裝後以 `pulse upgrade trust --install` 或組態管理放入；(b) 維持 `%ProgramData%\Pulse\allowed_signers` 並靠 ACL 檢查；MSI 屬性 `ALLOWEDSIGNERS=` 視 wixl 是否支援自訂動作 | **(a)**；`ALLOWEDSIGNERS=` 在階段 0 確認 wixl 的能力後再決定是否提供 |
| U9 | 支援的 Windows 版本 | Windows Server 2022、2025、Windows 11 為基本；Windows Server 2019、Windows 10、Server 2016 是否列入 | 預覽只列實際驗證過的（預期為 Server 2022、2025、Windows 11）；2019 視客戶需求在 VM 補做；2016 以前不列入 |
| U10 | 新的 `install.upgrade` 值 `insecure_path` | 新增／沿用 `unsupported` | 新增（舊版 Kairos 會顯示「其他」，行為安全），讓主控台能明確告訴管理者原因 |
| U11 | 階段 1（啟動器與槽、沒有升級行為）是否先隨下一個版本出貨 | 先出貨／等階段 4 一起 | **先出貨**（標示為配置變更）：讓「手動升級一次」提早發生，也讓啟動器在沒有升級風險的情況下先累積執行時間。前提是階段 0、1 的 CI 通過 |
| U12 | ADR-0003 的 Q1～Q6（第 4.11 節） | 由 ADR-0003 決定 | 本 ADR 的建議附在各題之後；Q1 的答案會影響啟動器是否需要內含簽章驗證 |

---

## 9. 參考資料

### 專案內

- [`crates/kairos-proto/REMOTE.md`](../../crates/kairos-proto/REMOTE.md)：第 2.1、2.4 節（`install`、`upgrade_offer`）、第 4.2～4.6、4.8、4.10 節、第 5 節、第 12.4 節、第 17 節（特別是 17.2 第 12 項、17.3 第 1 項、17.5）、第 18 節
- [`pulse/src/remote/install.rs`](../../pulse/src/remote/install.rs)、[`pulse/src/remote/upgrade/`](../../pulse/src/remote/upgrade/)（`mod.rs`、`helper.rs`、`files.rs`、`testsign.rs`）
- [`pulse/src/winsvc.rs`](../../pulse/src/winsvc.rs)、[`pulse/src/file_perms.rs`](../../pulse/src/file_perms.rs)、[`pulse/src/status_file.rs`](../../pulse/src/status_file.rs)
- [`crates/kairos-proto/src/release/mod.rs`](../../crates/kairos-proto/src/release/mod.rs)（`artifact_kind`）、[`kairos/src/remote/rollout.rs`](../../kairos/src/remote/rollout.rs)（資格分類）
- [`pulse/packaging/windows/pulse.wxs`](../../pulse/packaging/windows/pulse.wxs)、[`packaging/build-release-common.sh`](../../packaging/build-release-common.sh)、[`pulse/packaging/INSTALL.md`](../../pulse/packaging/INSTALL.md) A.3
- [`.github/workflows/ci.yml`](../../.github/workflows/ci.yml)、[`tests/systemd/`](../../tests/systemd/)、[`RELEASING.md`](../../RELEASING.md) 第 3、4 節、[`CHANGELOG.md`](../../CHANGELOG.md)「支援期限」與 [0.8.0]、[0.10.0]

### 外部（2026-11-12 查閱）

- Microsoft Learn：[MoveFileExW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw)（`MOVEFILE_REPLACE_EXISTING`、`MOVEFILE_WRITE_THROUGH`；目的地是既有目錄時回報錯誤；沒有關於原子性的承諾）
- Microsoft Learn：[ReplaceFileW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-replacefilew)（三個失敗代碼各自的殘留狀態；`REPLACEFILE_WRITE_THROUGH`「is not supported」）
- Microsoft Learn：[SERVICE_FAILURE_ACTIONSW](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/ns-winsvc-service_failure_actionsw)（失敗的定義、失敗次數與 `dwResetPeriod`）、[SERVICE_FAILURE_ACTIONS_FLAG](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/ns-winsvc-service_failure_actions_flag)（非 0 結束代碼；「takes effect the next time the system is started」）
- Microsoft Learn：[Windows Installer File Versioning Rules](https://learn.microsoft.com/en-us/windows/win32/msi/file-versioning-rules)（「Nonversioned Files are User Data」）
- GitHub Docs：[GitHub-hosted runners](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)（Windows runner 的標籤；以系統管理員執行、UAC 關閉）
- Android Open Source Project：[A/B (seamless) system updates](https://source.android.com/docs/core/ota/ab)
- Chromium：[Updater design doc](https://chromium.googlesource.com/chromium/src/+/main/docs/updater/design_doc.md)
- Squirrel.Windows：[Install process](https://github.com/Squirrel/Squirrel.Windows/blob/develop/docs/using/install-process.md)
- Elastic：[Upgrade Elastic Agent](https://www.elastic.co/docs/reference/fleet/upgrade-elastic-agent)
- Fleet：[Update agents](https://fleetdm.com/docs/using-fleet/update-agents)
- Tailscale：[`clientupdate/clientupdate_windows.go`](https://github.com/tailscale/tailscale/blob/main/clientupdate/clientupdate_windows.go)
- `%ProgramData%` 的權限與提權：[CERT/CC：Finding Privilege Escalation Vulnerabilities in Windows using Process Monitor](https://vuls.cert.org/confluence/display/Wiki/2021/06/21/Finding+Privilege+Escalation+Vulnerabilities+in+Windows+using+Process+Monitor)、[CyberArk：Anti-Virus Vulnerabilities](https://www.cyberark.com/resources/threat-research/anti-virus-vulnerabilities-who-s-guarding-the-watch-tower)

### 未能查證、文中標為待驗證的項目

1. 各 Windows 版本 `%ProgramFiles%`、`%ProgramData%`、磁碟根目錄的確切預設 ACE（只有第二手來源；階段 0 以 `icacls` 實測）。
2. NTFS 上 `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)` 在斷電時的實際行為（文件沒有承諾；設計以備份與掃描容錯，並以第 6.2 節的斷電測試驗證）。
3. wixl 0.106 對自訂動作、`RemoveFolderEx`、`MsiFileHash`、`MajorUpgrade` 排程的支援與產出。
4. GitHub 代管的 Windows runner 上 Microsoft Defender 即時保護是否開啟。
5. AppLocker／WDAC 的預設規則對 `%ProgramFiles%` 下新增的未簽章執行檔的處理；Defender 對「每次升級出現在新路徑的未簽章執行檔」的反應。
6. session 0、沒有主控台時通知子行程優雅結束的最佳方式（`GenerateConsoleCtrlEvent` 是否可用）。
7. `SERVICE_FAILURE_ACTIONS_FLAG` 的變更是否真的要到下次開機才生效，以及對現行 `ensure_failure_actions()` 的影響。
8. Elastic Agent 與 Fleet Orbit 在 Windows 上的版本目錄與指標的實作細節（只確認了官方文件描述的行為；目錄配置來自社群討論或未查證）；Telegraf 沒有內建自我更新的說法未查證。
9. Windows Server／Windows 11 評估版的授權條款是否允許本文所述的測試用途與有效天數；Windows 內建 `tar.exe` 可用的最低版本。
10. ReFS、Server Core、非英文語系下的行為。
