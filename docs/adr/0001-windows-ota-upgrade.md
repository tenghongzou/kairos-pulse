# ADR-0001：Pulse 在 Windows 上的遠端升級採用 OTA 式的版本槽機制

| 項目 | 內容 |
|---|---|
| 狀態 | 已核定（2026-11-13，使用者核定；依 2026-11-12 外部審查修訂後的版本。待決事項除另有註明外，皆採文內建議）；修訂紀錄見第 10 節 |
| 日期 | 2026-11-12 |
| 決策者 | 使用者（2026-11-13 核定） |
| 取代／修訂 | 核定後取代 [`REMOTE.md`](../../crates/kairos-proto/REMOTE.md) 第 4.4.4 節的 Windows 替換方式（就地 `msiexec`／zip 改名），並修訂第 2.1、4.2.1、4.3、4.4.2、4.5、4.6、12.4、18.2、18.3 節 |
| 相關 | [ADR-0003](0003-rollback-bundle-trust.md)（回滾檔的信任；本文遵守其 INV-1～INV-11 與第 7 節的結論）；[ADR-0002](0002-rollout-console-design.md)（推送的主控台設計；其資格預覽與「回滾需要的檔案」統計必須排除 `slot` 與 `tar`，並顯示 `insecure_path`、`launcher_abi`——核定後兩份 ADR 需互相引用）；審查 [`reviews/0001-codex-review.md`](reviews/0001-codex-review.md) |

用詞約定：

- 「**待驗證**」：沒有在 Windows 上實際執行過，或沒有找到可引用的第一手來源。這些項目列在第 6 節的驗證計畫或第 8 節的待決事項，不應視為事實。
- 「**假設**」：產品或客戶面的前提，儲存庫中沒有資料可以佐證。
- 本文所有「一定」「不會」類的敘述都附條件；沒有附條件的保證請視為筆誤並回報。

---

## 1. 脈絡

### 1.1 問題

- Linux 的遠端升級已在 0.9.0 出貨（`pulse/src/remote/upgrade/`：`UpgradeAgent` 與特權輔助程式 `pulse upgrade apply`）。
- Windows 的遠端升級在 0.10.0 評估後沒有實作（`REMOTE.md` 第 18 節）：`pulse/src/remote/install.rs` 在非 Linux 平台一律回報 `install.upgrade = "unsupported"`，Kairos 的 `kairos/src/remote/rollout.rs` 把非 Linux 主機分類為 `unsupported_platform`。
- 使用者看過評估後的決定（2026-11-12）：**「要類似 OTA 機制」**。也就是 Windows 的遠端升級要做，而且採 OTA 式的做法（版本槽、下載到非使用中的槽、切換、啟動後健康確認、失敗自動切回），而不是 `REMOTE.md` 第 4.4.4 節原本規劃的就地執行 `msiexec` 覆蓋。細節由本 ADR 提出並建議。

### 1.2 現況（讀程式得到的事實）

| 項目 | 現況 | 出處 |
|---|---|---|
| 服務 | 服務 `Pulse`、`ownProcess`、自動啟動、LocalSystem（`account_name: None`；wxs 沒有指定帳號）；命令列 `"<安裝目錄>\pulse.exe" service run`（zip 安裝另帶 `--workdir`） | `pulse/packaging/windows/pulse.wxs`、`pulse/src/winsvc.rs` |
| 失敗復原（**與文件不符，見下方「既有缺陷」**） | 程式設定三個 `Restart`（延遲 10 秒）、重設期 86400 秒、非 0 結束代碼也算失敗 | `pulse/src/winsvc.rs` `set_failure_actions` |
| 檔案配置 | `C:\Program Files\Pulse\`：`pulse.exe`、文件、`pulse.toml`（`Permanent`）；**執行時的 `agent.id`、`identity\`、`buffer\`、`logs\` 也在安裝目錄**（服務的工作目錄）；只有 `status.json` 在 `%ProgramData%\Pulse\`（授予 Users 讀取） | `pulse.wxs`、`winsvc.rs`、`status_file.rs` |
| MSI | 在 Linux 容器內以 msitools 的 wixl 0.106 產生；`UpgradeCode` 固定；`MajorUpgrade` 不允許降版（較舊的 MSI 會被 Windows Installer 拒絕）；`ServiceControl` 為 `Stop="both" Remove="uninstall"`、**沒有 `Start`**；沒有自訂動作 | `pulse.wxs` |
| `ProductCode` | 由**去掉預發佈標記的數字版本**推導（`MSI_VERSION=${VERSION%%-*}`）。因此 `0.11.0-test.1` 與 `0.11.0-test.2`、`0.11.0` 的 `ProductCode` **相同**，只有 `PackageCode`（版本＋commit）不同 | `packaging/build-release-common.sh` |
| 執行檔 | `x86_64-pc-windows-gnu` 交叉編譯，沒有 PE 版本資源、**沒有 Authenticode 簽章**，建置可重現 | `RELEASING.md` 第 4 節、`CHANGELOG.md` [0.8.0] |
| 安裝方式偵測 | 執行檔在 `%ProgramFiles%\Pulse` → `msi`，否則 `zip` | `pulse/src/remote/install.rs` `windows_method` |
| 信任錨的預設位置 | Windows 為 `%ProgramData%\Pulse\allowed_signers` | `pulse/src/config_manager.rs` `default_allowed_signers_path`、`REMOTE.md` 第 4.2.1 節 |
| ACL 檢查 | 已有 DACL 解析（`GetNamedSecurityInfoW`），**只看允許項目、不看擁有者、不看重新解析點、不計算有效權限**，目前只用來警告 | `pulse/src/file_perms.rs` |
| CI | `windows-latest` 上跑 `cargo fmt`／`clippy`／`test` 與 `kp-e2e`；MSI 與 zip 只在 Linux job 打包，**從來沒有在 Windows 上安裝過** | `.github/workflows/ci.yml` |
| 支援狀態 | 「Pulse 另有 Windows x86_64（0.8.0 起，**未在 Windows 實機驗證前不列入支援**）」 | `CHANGELOG.md`「支援期限」 |

**既有缺陷（與本 ADR 的決定無關，0.8～0.10 的 Windows 服務都受影響；全部尚未在 Windows 上實測）：**

| # | 缺陷 | 依據 | 影響 |
|---|---|---|---|
| X1 | 文件與程式註解寫「一天內最多 3 次」，實際不是。服務控制管理員在失敗次數超過動作陣列長度後會**重複最後一個動作**（「If N is greater than cActions, the service controller repeats the last action in the array」）。三個 `Restart` 的實際效果是**每 10 秒重新啟動、沒有次數上限**；86400 秒只是「連續一天沒有失敗才把計數歸零」 | [SERVICE_FAILURE_ACTIONSW](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/ns-winsvc-service_failure_actionsw) | 設定或憑證錯誤（結束代碼 3／4）時服務會無限重啟；Linux 的 unit 對這些結束代碼不重啟，兩個平台行為不同。`winsvc.rs` 的註解、`pulse service --help`、`CHANGELOG.md` [0.8.0] 的敘述都要更正 |
| X2 | `ensure_failure_actions()` 只檢查「是否已有任何動作」，不比對完整內容，也不單獨檢查「非 0 結束代碼也算失敗」的旗標 | `pulse/src/winsvc.rs` | 已有部分設定（例如管理者或 GPO 設過）的服務不會被修正 |
| X3 | 該旗標的變更「takes effect the next time the system is started」；MSI 註冊的服務是在第一次啟動時才補上設定 | [SERVICE_FAILURE_ACTIONS_FLAG](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/ns-winsvc-service_failure_actions_flag) | 安裝後到第一次重開機之間，非 0 結束代碼可能不會觸發失敗復原（待驗證） |
| X4 | wxs 的 `ServiceControl` 沒有 `Start`：MSI 升級或修復會停止服務而**不會再啟動** | `pulse.wxs` | 以 SCCM 升級後服務可能停到下次開機或手動啟動（待驗證；INSTALL 目前只提到 `pulse enroll` 會重新啟動） |

這四項的處理方式見第 4.4 節與第 8 節 U13；不論本 ADR 是否核定都應修正。

### 1.3 限制

1. **沒有 Windows 實機**。`REMOTE.md` 第 18.1 節的結論「沒有實機就不出貨」仍然成立；本 ADR 不推翻它，而是改變「實機」的取得方式（第 6 節）。
2. **信任邊界是 ACL，而且 Windows 目前沒有特權分離**。
   - Linux 靠「root 擁有、其他人不可寫、`O_NOFOLLOW`、`st_nlink`」；Windows 要檢查擁有者、DACL 與重新解析點，寫錯不會有錯誤訊息，結果是本機提權（第 18.1 節第 2 項）。
   - `%ProgramData%` 可由非特權使用者寫入，特權行程在其中建立的目錄若沒有另外設定 ACL，常成為提權路徑（[CERT/CC](https://vuls.cert.org/confluence/display/Wiki/2021/06/21/Finding+Privilege+Escalation+Vulnerabilities+in+Windows+using+Process+Monitor)、[CyberArk](https://www.cyberark.com/resources/threat-research/anti-virus-vulnerabilities-who-s-guarding-the-watch-tower)）。Microsoft 的一般性建議是應用程式裝在 Program Files、其他位置必須自行正確設定 ACL（[Windows Shell Security Considerations](https://learn.microsoft.com/en-us/windows/win32/shell/sec-shell)，審查引用）。各 Windows 版本的確切預設 ACE 沒有第一手文件，需以 `icacls` 實測（待驗證）。
   - 服務是 LocalSystem，**Pulse 本身就是特權行程**；Linux 的「非特權 Pulse ＋ root 輔助程式」的分權在 Windows 上不存在。ACL 只能擋「一般本機使用者 → LocalSystem」，**擋不住同為 LocalSystem 的代理程式**。這直接限制了本設計能承諾的範圍（第 4.6 節）。
3. **MSI 產品碼與「應用程式與功能」的版本顯示**。任何不經 `msiexec` 的升級都會讓安裝紀錄與實際執行的版本不同。另外，沒有版本資源的執行檔屬於「nonversioned file」，規則是「修改時間晚於建立時間就視為使用者資料、不覆寫」（[File Versioning Rules](https://learn.microsoft.com/en-us/windows/win32/msi/file-versioning-rules)）——在 MSI 管理的目錄中就地改寫 `pulse.exe` 會讓之後的 MSI 修復／升級行為難以預測（wixl 是否產生 `MsiFileHash`、`MajorUpgrade` 的排程位置，待驗證）。
4. **服務帳號**。LocalSystem、session 0、沒有互動桌面。
5. **防毒與應用程式控制**。新寫入的執行檔會被即時掃描，期間開啟、改名、刪除可能失敗或延遲；未簽章的執行檔可能被隔離或被 WDAC／AppLocker 阻擋（待驗證）。
6. **重開機與斷電**。`MoveFileEx` 的文件只說明 `MOVEFILE_WRITE_THROUGH` 會等到搬移寫入磁碟，**沒有承諾斷電後的原子性或名稱繫結的持久性**（[MoveFileExW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw)）。設計不能假設任何單一檔案操作在斷電時完整。
7. **打包工具**。wixl 的功能遠少於 WiX；自訂動作、`RemoveFolderEx`、權限設定、`ServiceControl Start` 的支援都待驗證。方案不應依賴 wixl 沒有驗證過的功能。

---

## 2. 決策驅動因素

依優先順序：

| # | 因素 | 說明 |
|---|---|---|
| D1 | **安全** | 只安裝本機信任錨驗證過的官方發佈（`REMOTE.md` 第 4.2 節）；不引入「本機使用者 → LocalSystem」的提權路徑；不是遠端執行通道（第 5.5 節）；信任邊界盡量靠系統預設就安全的位置；**承諾的範圍必須與實際的權限隔離相符** |
| D2 | **可回滾性** | 新版不健康、升級過程當機或斷電之後，主機上保有先前確認過的版本可以啟動，而且**回滾不需要網路、不需要 Kairos 提供任何檔案** |
| D3 | **可驗證性** | 能在沒有實機的情況下自動化驗證的比例越高越好；失敗時能分辨是哪一層（MSI、服務、升級）的問題 |
| D4 | **與 Linux 流程和協定的一致性** | 沿用 `upgrade_offer`／`upgrade_status`、`SHA256SUMS`＋`allowed_signers`、第 4.5 節的健康條件、第 4.6 節的狀態與代碼、ADR-0003 的不變條件 |
| D5 | **維運成本** | 客戶的 SCCM／GPO 流程、MSI 安裝紀錄、磁碟用量、診斷方式；我們自己的打包與發佈流程 |

---

## 3. 考慮的方案

業界做法的對照：

| 產品 | 做法 | 來源 |
|---|---|---|
| Android A/B | 兩個槽；更新寫入非使用中的槽；槽有 active／bootable／successful 屬性；開機後由 `update_verifier` 標記成功，開不起來時 bootloader 回到舊槽 | [A/B (seamless) system updates](https://source.android.com/docs/core/ota/ab) |
| Chromium updater | 每個版本解到自己的版本目錄；新版先自我測試（qualification）再啟用（activation），啟用以鎖與「swapping」旗標保護、中斷後可接續；舊版自行移除 | [Chromium updater design doc](https://chromium.googlesource.com/chromium/src/+/main/docs/updater/design_doc.md) |
| Squirrel.Windows | `app-<版本>\` 並存目錄＋`packages\`，由 `Update.exe` 管理 | [Squirrel install process](https://github.com/Squirrel/Squirrel.Windows/blob/develop/docs/using/install-process.md) |
| Elastic Agent | 升級後進入「監看」狀態，失敗回到升級前的版本；回滾所需的資料在磁碟保留七天 | [Upgrade Elastic Agent](https://www.elastic.co/docs/reference/fleet/upgrade-elastic-agent)（版本目錄與指標的實作細節未查證） |
| Fleet 的 fleetd（Orbit） | Orbit 是常駐的包裝行程，以 TUF 更新並啟動 osquery | [Fleet：Update agents](https://fleetdm.com/docs/using-fleet/update-agents)（Windows 上的目錄配置來自社群討論，未查證） |
| Tailscale | **就地 MSI**：下載 MSI 到 `%ProgramData%\Tailscale\MSICache`、驗證 Authenticode、把自己複製成 `tailscale-updater-*.exe` 後結束，由複本執行 `msiexec /i … /quiet /norestart`；失敗時假設是降版而先解除安裝再重試，沒有自動回到舊版 | [`clientupdate_windows.go`](https://github.com/tailscale/tailscale/blob/main/clientupdate/clientupdate_windows.go) |

### 方案 A：就地 `msiexec` 升級（`REMOTE.md` 第 4.4.4 節的原規劃）

- **升級步驟**：Pulse 下載並驗證 MSI → 把目前的 `pulse.exe` 複製成 `pulse-upgrader.exe`、脫離服務的工作物件啟動 → 升級程序重新驗證 → `msiexec /i 新版.msi /qn /norestart` → MajorUpgrade 停止並移除舊服務、安裝新檔案與服務 → 升級程序啟動服務 → 健康確認。
- **失敗時的狀態**：取決於 Windows Installer 交易在哪一步失敗。一般的安裝失敗由 Windows Installer 的回復指令碼復原（[Rollback Installation](https://learn.microsoft.com/en-us/windows/win32/msi/rollback-installation)，審查引用、本次未重讀）；回復本身失敗時可能沒有服務、沒有執行檔。1618、3010 要另外處理。
- **斷電／當機的復原**：MSI 交易中斷電後的狀態與下一次 `msiexec` 的行為**沒有查到第一手依據（未驗證）**。升級程序是一次性的行程，重開機後要靠服務啟動時把它叫起來——但此時服務可能已被 MajorUpgrade 移除。
- **回滾**：以舊版 MSI 降版安裝，需要 `AllowDowngrades="yes"`，且需要 Kairos 提供舊版 MSI（`rollback_artifact`）——正是 ADR-0003 要處理的信任問題；回滾本身又是一次完整的 MSI 交易。
- **權限模型**：LocalSystem 寫入 `%ProgramData%\Pulse\upgrade\<id>\`（必須自己設定並檢查 ACL），再以 LocalSystem 執行 `msiexec`。
- **與 MSI 安裝紀錄的關係**：最好——「應用程式與功能」、SCCM 偵測都與實際版本一致。
- **既有安裝的遷移**：MSI 安裝的主機手動裝一次新版 MSI；zip 安裝不適用。
- **評價**：D5 最好；D2、D3 最差。使用者已明確不選這條路。

### 方案 B：zip 就地替換執行檔

- **升級步驟**：停止服務 → `pulse.exe` 改名為 `pulse.exe.prev` → 寫入新版 → 啟動服務 → 健康確認。
- **失敗時的狀態**：改名之後、寫入完成之前失敗時，安裝目錄沒有可啟動的 `pulse.exe`。
- **斷電／當機的復原**：要靠一次性的升級程序在重開機後把 `.prev` 改回來，而服務此時起不來。`ReplaceFile` 可以把「改名＋換上」合成一步並留備份，但它的三個失敗代碼各自對應不同的殘留狀態（[ReplaceFileW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-replacefilew)），不是原子操作。
- **回滾**：`pulse.exe.prev` 改名回來；不需要外部檔案。
- **權限模型**：LocalSystem 改寫安裝目錄；暫存目錄的 ACL 問題與方案 A 相同。
- **與 MSI 安裝紀錄的關係**：對 MSI 安裝的主機最差——改寫 MSI 管理的 key path 檔案（第 1.3 節第 3 項）。
- **既有安裝的遷移**：zip 安裝手動換一次執行檔。
- **評價**：實作量最小，但有「沒有可啟動的執行檔」的時間窗。`REMOTE.md` 第 18.4 節 (c) 認為「多數 Windows 部署使用 MSI」，這是**假設**——儲存庫中沒有部署資料可以佐證；若假設成立，只做 zip 的效益有限。

### 方案 C：A/B 版本槽＋常駐啟動器（launcher）切換　←　建議

- **概念**：服務的進入點是一個**小而少變動的啟動器**，只負責：依自己保存的狀態選槽、把槽中的 `pulse.exe` 當子行程啟動並監看、切換、健康確認、切回。每個版本放在自己的槽目錄，升級是「在旁邊建立新槽、寫一筆新的狀態紀錄」，使用中的槽與啟動器在升級過程中不被改寫。
- **升級步驟**：代理程式下載並驗證發佈檔 → 啟動器啟動輔助程式 → 輔助程式重新驗證、建槽、預檢、封存 → 啟動器記下「試用中」→ 停止子行程、啟動新槽 → 健康確認 → 通過則提交（新槽成為 `active`、舊槽成為 `previous`）；不通過則放棄試用、啟動原本的 `active`。
- **失敗時的狀態**：建槽階段失敗 → 刪除暫時目錄，舊版一直在執行，沒有停機。切換後不健康 → `active` 沒有改過，啟動器啟動它即回到舊版。
- **斷電／當機的復原**：啟動器是服務本體，開機後由服務控制管理員啟動（前提：啟動器本身完好、沒有被應用程式控制或防毒阻擋）；它依狀態紀錄決定啟動 `active` 或繼續／放棄試用。狀態紀錄的損毀以第 4.3 節的規則處理。
- **回滾**：本機的 `active`，不需要網路、不需要 `rollback_artifact`、不經 Windows Installer。
- **權限模型**：與升級信任有關的路徑都放在安裝根目錄下（MSI 安裝為 `%ProgramFiles%\Pulse`），不在 `%ProgramData%` 放任何會被信任的檔案；ACL 檢查作為縱深防禦。**在代理程式仍是 LocalSystem 的期間，這個模型只擋一般本機使用者，不擋代理程式本身**（第 4.6 節）。
- **與 MSI 安裝紀錄的關係**：MSI 降級為 bootstrap（安裝啟動器、註冊服務、帶入種子）。OTA 之後 MSI 紀錄的版本不等於執行中的版本（第 4.10 節；這是此方案最主要的維運代價）。
- **既有安裝的遷移**：手動（或 SCCM）安裝一次含啟動器的新版 MSI／zip；服務的 `ImagePath` 不必改。
- **評價**：D2、D3 最好；D4 良好；D1 在「低權限代理程式」完成前只有部分達成；D5 的代價是版本顯示與多一個要維護的啟動器。

### 方案 D：獨立的更新服務（updater service）

- **概念**：另外註冊一個 LocalSystem 的服務負責替換、重新啟動、健康確認、回滾；`Pulse` 服務可降為低權限帳號。相當於 Omaha／Chromium updater 的形態。
- **升級步驟／失敗／斷電／回滾**：取決於它採用哪一種替換方式（A、B 或 C）；更新服務開機自動啟動，可以接續未完成的升級。
- **權限模型**：最清楚——只有更新服務是 LocalSystem，恢復 Linux 的分權；請求通道（具名管道或目錄）的 ACL 成為新的信任邊界。
- **與 MSI 安裝紀錄的關係**：MSI 多註冊一個服務；其餘同替換方式。
- **既有安裝的遷移**：手動裝新版 MSI；zip 安裝多一次服務註冊。
- **評價**：它回答「誰來替換」，沒有回答「怎麼替換」。分權是真正的優點，但把代理程式降為低權限帳號本身需要實機驗證（磁碟 I/O、行程清單、SMBIOS 等蒐集在低權限下的行為，`CHANGELOG.md` [0.8.0] 已列為未驗證）。

### 方案 E：組合

| 組合 | 說明 | 評價 |
|---|---|---|
| **E1：C＋MSI 作為 bootstrap** | 方案 C，MSI／zip 只負責第一次安裝、啟動器與種子的更新 | **預覽階段的建議** |
| E2：C＋A | 槽升級之後，背景再執行 `msiexec` 讓安裝紀錄一致 | 把方案 A 的風險帶回來，只為了版本顯示；不建議 |
| **E3：E1＋低權限代理程式** | 啟動器（與輔助程式）是唯一的特權元件；代理程式以低權限身分執行，寫不到槽、狀態紀錄、啟動器與信任錨 | **進入「支援」的門檻**（第 4.6、7 節）；可以由啟動器降權啟動子行程，不一定要多一個服務 |

### 比較總表

| 面向 | A `msiexec` | B zip 就地 | **C／E1 槽＋啟動器** | D 更新服務 |
|---|---|---|---|---|
| 升級時的停機 | 數十秒（MSI 交易；估計） | 數秒（估計） | 數秒（只有切換；估計） | 依替換方式 |
| 升級過程中是否會改寫正在使用的版本 | 會 | 會 | **不會** | 依替換方式 |
| 回滾需要外部檔案 | 需要（舊版 MSI） | 不需要 | **不需要** | 依替換方式 |
| 斷電後由誰接續 | 一次性行程 | 一次性行程 | 服務本體（啟動器） | 更新服務 |
| 信任邊界的位置 | `%ProgramData%`＋自訂 ACL | 同左 | `%ProgramFiles%` 預設 ACL＋檢查 | 請求通道的 ACL |
| 代理程式被入侵或嚴重出錯時能否保住舊版 | 不能 | 不能 | E1 不能；**E3 可以** | 可以（若代理程式降權） |
| MSI 紀錄與實際版本 | **一致** | 不一致（且改寫 MSI 的檔案） | 不一致（不改寫 MSI 的檔案） | 依替換方式 |
| CI（無實機）可驗證的比例 | 低 | 中 | **高** | 中 |
| 粗估工作量（工程與自動測試） | 原估 1.5 人週，不含無法估計的實機除錯 | 約 1 人週 | 8～12 人週（第 7 節；不含 E3） | C 之上另計 |

---

## 4. 決策（建議）

**採用方案 E1 作為預覽：A/B 版本槽＋獨立的小型啟動器；MSI／zip 只作為第一次安裝、啟動器與種子更新的 bootstrap。不實作就地 `msiexec` 升級與 zip 就地替換。進入「支援」前必須完成 E3（代理程式以低權限執行）。**

以下是建議的具體設計；核定後搬入 `REMOTE.md`。

### 4.1 元件與角色

| 元件 | 位置 | 角色 | 由誰更新 |
|---|---|---|---|
| **啟動器** | `<根>\pulse.exe`（服務的 `ImagePath` 不變：`service run`） | 服務進入點；選槽、啟動並監看子行程、切換、健康確認、切回；把其他命令列轉送給 `active` 槽 | **只由 MSI／zip**；OTA 不改它 |
| **種子** | `<根>\seed\pulse.exe`＋`seed\seed.json` | 隨 MSI／zip 帶入的完整代理程式；啟動器以它建立槽 | 只由 MSI／zip |
| **代理程式** | `<根>\slots\<槽>\pulse.exe` | 原本的 Pulse：連線、回報、集中設定、下載並第一次驗證升級 | OTA，或由種子 |
| **輔助程式** | `active` 槽的 `pulse.exe upgrade apply --request <固定路徑>` | 重新驗證、建槽、預檢（對應 Linux 的 `pulse-upgrade.service`） | 隨代理程式 |

- `<根>`：MSI 為 `%ProgramFiles%\Pulse`；zip 為解開的目錄。
- **啟動器是獨立的小型執行檔**（審查意見，採納；第 8 節 U6）：由獨立的 crate 建置，安裝時命名為 `pulse.exe` 放在 `<根>`。理由：它是單點、只能靠 MSI 修正，應該只帶必要的程式與相依（服務控制、行程與工作物件、檔案與 ACL、SHA-256、JSON），不帶網路、TLS、設定合併、蒐集等代理程式的程式碼。沿用 `pulse.exe` 這個檔名與 `service run` 的命令列，是為了讓既有服務的 `ImagePath`、文件中的路徑與以檔案存在為準的 SCCM 偵測規則不必改。代價：安裝包多一份代理程式（種子），wxs 多一個只含一般檔案的元件（不需要 wixl 未驗證的功能）。
- 啟動器不做簽章驗證、不解壓縮、不預檢——這些在輔助程式（隨 OTA 更新）。ADR-0003 Q1 的結論（回滾當下不重驗簽章）讓這個分工成立。
- 啟動器與代理程式之間的約定（命令列、狀態紀錄與 `slot.json` 的格式、停止方式、健康訊號）以整數 `launcher_abi` 版本化，相容規則見第 4.9 節。

### 4.2 目錄配置

```
<根>\                                 （MSI：C:\Program Files\Pulse\）
├─ pulse.exe                          啟動器（MSI 管理；OTA 不寫）
├─ seed\pulse.exe、seed\seed.json     種子（MSI 管理；OTA 不寫）
├─ pulse.toml、agent.id、identity\、buffer\、logs\     不變：服務的工作目錄，所有槽共用
├─ allowed_signers                    信任錨（新的預設位置，第 4.5 節）
├─ state\
│  ├─ 0000000041.json …               狀態紀錄：每次變更新增一個檔案，檔名是單調遞增的 generation（第 4.3 節）
│  └─ trial-progress.json             試用的累計確認時間（遺失或損毀時視為已逾時）
├─ slots\
│  ├─ 0.11.0-1a2b3c4-9f3a6c21d0b7\    一個槽：<版本>-<commit>-<發佈檔 SHA-256 的前 12 個十六進位字元>
│  │  ├─ pulse.exe …
│  │  ├─ slot.json                    封存標記：最後寫入、之後不再修改；列出槽內每個檔案的 SHA-256
│  │  └─ release\                     所屬發佈的 SHA256SUMS、SHA256SUMS.sig、清單、record.json（ADR-0003 Q3 的格式）
│  └─ .tmp-<upgrade_id>\              建槽中的暫時目錄
└─ upgrade\
   ├─ staging\<upgrade_id>\           代理程式下載的檔案
   ├─ work\<upgrade_id>\              輔助程式的私有複本、state.json、ready.json
   ├─ request.json、result.json       與 Linux 相同的交付與結果檔
   ├─ health.json                     試用中的代理程式寫給啟動器的健康訊號（第 4.7 節）
   └─ failed\<槽>\                    失敗試用的診斷紀錄（只留中繼資料，不留執行檔）
```

- **槽 ID 納入發佈檔的雜湊**（審查 2.6，採納）：同一個版本與 commit 的不同重建、重新簽署不會撞名。種子槽以 `seed.json` 記錄的代理程式執行檔 SHA-256 取代發佈檔雜湊，並加前綴 `s`（例如 `0.11.0-1a2b3c4-s5e2c…`）。顯示用的版本與 commit 另存在 `slot.json`。
- **建槽時遇到同名目錄**：若它是目前狀態紀錄引用的槽且完整性檢查通過 → 直接沿用（同一份內容）；否則視為殘留，先刪除再建。
- **失敗的試用槽不保留執行檔**（原設計保留 7 天；依審查 2.2、2.6 與 ADR-0003 INV-10 修改）：切回並確認舊版健康後立即刪除槽目錄，只把 `slot.json` 與失敗原因搬到 `upgrade\failed\<槽>\`。日誌本來就在共用的 `logs\`。因此磁碟上不存在「失敗但仍可被選中」的槽。
- **為什麼是版本命名的槽目錄而不是固定的 `a\`、`b\`**：語意上是 A/B（`active` 與 `previous`），但新版永遠寫進新的目錄，不必就地覆寫舊槽的檔案；被防毒軟體開啟中的舊檔案只會讓清除延後（可重試）。
- **為什麼放在 `<根>` 而不是 `%ProgramData%`**：第 1.3 節第 2 項。審查建議評估「由 MSI 建立 `%ProgramData%\Pulse-Upgrade` 並明確套用 ACL」；它符合 Windows 把可變資料放在 ProgramData 的慣例，但依賴 wixl 設定權限的能力（待驗證），而且執行時才建立會有被搶先建立的競態。**第一版不採用**；E3 把代理程式可寫的資料（`staging\`、`buffer\`、`logs\`）與特權資料分開時再評估（第 4.6 節）。
- 槽之間共用設定、身分、緩衝與日誌（與 Linux 相同）。新版改變本機資料格式而舊版讀不懂時切回會失敗，這是與 Linux 相同的殘餘風險（第 5.3 節）。

**磁碟空間**（審查意見，重算；取代 `REMOTE.md` 第 4.6 節的 `3 × artifact.size + 64 MiB`）。設 `Z` 為發佈檔（zip）大小、`E` 為解開後一個槽的大小：

| 時點 | 佔用 |
|---|---|
| 平時 | 種子 `E`＋`active` `E`＋`previous` `E` |
| 升級的峰值（建槽完成、尚未切換） | 平時的 `3E`＋`staging` `Z`＋`work` 的複本 `Z`＋新槽 `E` ＝ `4E + 2Z` |
| 試用中 | `4E`（`staging`、`work` 的發佈檔在建槽成功後即刪除） |
| 提交後 | `3E`（舊的 `previous` 刪除） |

- 接受提議前的檢查：可用空間 ≥ `2Z + E + 64 MiB`（`E` 以清單或 zip 目錄的未壓縮大小計；上限見第 4.5 節的解壓縮限制）。
- 清理順序（空間不足時依序嘗試，每一步後重新檢查）：（1）狀態紀錄沒有引用的槽、`.tmp-*`、過期的 `staging\`／`work\`、`upgrade\failed\` 中較舊的紀錄；（2）刪除 `previous`（寫入一筆狀態紀錄記下「為了空間放棄手動回滾目標」）；（3）仍不足 → `declined`（`PLS-W8412`）。`active` 與種子不在清理範圍內。

### 4.3 狀態紀錄：單調遞增的 generation、寫入順序與恢復

（審查 2.2 與 ADR-0003 第 7.3 節第 1、5 點，採納並改寫。原設計的 `current.json`＋`.bak`＋「掃描槽目錄選版本最高且 `confirmed` 的槽」有三個問題：最高版本不等於最後一次提交；`confirmed` 與指標不是同一次寫入；失敗的試用槽可能被選中。以下設計**取消掃描選槽**、取消 `slot.json` 的 `confirmed` 欄位，`slot.json` 封存後不再修改。）

**格式**：`state\<generation>.json`，generation 是 10 位數、從 1 開始、每次加一。每一筆是**完整的狀態**，寫入後不再修改：

```json
{
  "envelope": 1,
  "generation": 41,
  "active": { "slot": "0.11.0-1a2b3c4-9f3a6c21d0b7", "exe_sha256": "…" },
  "schema": "kairos-pulse.launcher-state/v1",
  "previous": { "slot": "0.10.1-9f8e7d6-22c1…", "exe_sha256": "…" },
  "trial": { "slot": "…", "exe_sha256": "…", "source": "ota", "upgrade_id": "…",
             "health_timeout_secs": 600, "starts": 1 },
  "reason": "trial_start",
  "launcher_sha256": "…",
  "seed_seen_sha256": "…",
  "revoked": [],
  "checksum": "<以上內容的 SHA-256>"
}
```

- `envelope`、`generation`、`active`、`checksum` 四個欄位是**永久不變的信封**：任何版本的啟動器都必須能只靠它們啟動 `active`（第 4.9 節的相容規則）。
- `reason`：`initial_seed`、`trial_start`、`trial_restart`（`starts` 加一）、`commit`、`rollback`、`manual_rollback`、`supersede`（本機安裝取代進行中的 OTA）、`drop_previous`。
- **`active` 只在 `initial_seed`、`commit`、`manual_rollback` 三種紀錄中改變**；`manual_rollback` 把被退掉的槽從 `previous` 移除並刪除該槽。因此「最後一筆有效紀錄的 `active`」就是最後一次成功提交或管理者手動選定的槽，不需要比較版本高低（符合 INV-1、INV-10）。

**寫入一筆紀錄的步驟**：（1）在 `state\` 寫 `<generation>.json.tmp`；（2）`FlushFileBuffers`；（3）`MoveFileExW(tmp → <generation>.json, MOVEFILE_WRITE_THROUGH)`——目的地是**新的檔名**，不取代既有檔案；目的地已存在時視為程式錯誤並停止升級；（4）成功後才執行這筆紀錄所代表的動作；（5）之後刪除更舊的紀錄，只保留最近 8 筆。

**讀取規則**（啟動器每次啟動）：列出 `state\*.json`，從 generation 最大的開始，取第一筆「可解析、`checksum` 相符、`active` 槽通過完整性檢查（第 4.5 節 V5）」的紀錄。比它新的紀錄若無效，記 `ERROR`、保留檔案供診斷、不刪除。**沒有任何有效紀錄時不掃描 `slots\`**：以種子重新建立初始槽（`initial_seed`，generation 接續目錄中最大的號碼加一），並回報高嚴重度事件——這是 INV-5 的「退到更保守的來源」：種子是本機管理者安裝的版本（INV-8）。

**各轉換的寫入順序與當機點**（「當機」含斷電；「恢復結果」的前提是儲存裝置遵守排清要求、檔案系統在重開機後一致——這個前提本身待第 6.2 節的斷電測試驗證）：

| 轉換 | 順序 | 當機點 → 重新啟動後的結果 |
|---|---|---|
| 初次安裝 | ① 以種子建槽（暫時目錄 → 寫 `slot.json` → 改名）② 寫 `initial_seed` ③ 啟動 | ① 中：清除 `.tmp-*`，重做。①②之間：沒有有效紀錄 → 重做；同名的槽完整則沿用。②後：啟動 `active` |
| 開始試用（OTA 或種子） | ① 輔助程式／啟動器建槽並封存 ② 寫 `trial_start`（`starts: 0`）③ 清除 `trial-progress.json` ④ 停止子行程 ⑤ 寫 `trial_restart`（`starts: 1`）⑥ 啟動試用槽 | ①中：清除暫時目錄；`request.json` 還在 → 重新建槽。①②之間：最新紀錄沒有試用 → 啟動 `active`；之後重新觸發，槽已存在則沿用。②～⑤之間：有試用、`starts: 0` → 從 ④ 接續。⑤後：有試用、`starts ≥ 1` → 再寫一筆 `trial_restart`；超過上限則走「放棄試用」 |
| 提交 | ① 寫 `commit`（`active ← 試用槽`、`previous ← 原 active`、`trial ← 無`）② 寫 `result.json` ③ 刪除舊的 `previous` 槽與暫存 | ①前：仍是試用中 → 繼續確認（多算一次啟動）。①後：已提交；②③是可重做的清理（沒有被最新紀錄引用的槽一律刪除） |
| 放棄試用（切回） | ① 停止子行程 ② 寫 `rollback`（`trial ← 無`，`active` 不變）③ 啟動 `active` 並確認健康 ④ 寫 `result.json`、把試用槽的中繼資料搬到 `upgrade\failed\`、刪除試用槽 | ②前：仍是試用中 → `starts` 加一後多半直接超過上限，再走一次本流程。②後：啟動 `active`；④是可重做的清理 |
| 手動回滾 | ① 寫 `manual_rollback`（`active ← previous`、`previous ← 無`）② 重新啟動子行程 ③ 刪除被退掉的槽 | ①前：沒有變更。①後：啟動新的 `active`；③可重做 |
| `trial_start` 的紀錄在斷電後遺失（寫入回報成功但未持久化） | — | 讀到前一筆（沒有試用）→ 啟動 `active`；`request.json` 還在 → 重新進入試用。結果是多做一次，不會選到未確認的槽 |
| `commit` 的紀錄在斷電後遺失 | — | 讀到試用中的紀錄 → 重新確認；通過後再提交 |
| 最新數筆紀錄全部損毀 | — | 退到更舊的有效紀錄（最多回溯 8 筆）。可能回到較早的 `active`——它曾經被確認過，但可能已被清理而不通過完整性檢查，此時繼續往更舊的紀錄找，最後是種子 |

`trial-progress.json`（累計確認秒數）以就地取代的方式每 10 秒更新；它不是選槽的依據，**遺失或損毀時視為已逾時**（放棄試用，往保守的方向）。

**不用 junction／符號連結或服務的 `ImagePath` 當指標**：前者的切換不是單一步驟，而且重新解析點正是 ACL 檢查要拒絕的東西；後者沒有常駐的行程負責健康確認，而且 MSI 修復會把它改回去。

### 4.4 啟動器的行為與失敗復原

服務啟動（開機、服務控制管理員重新啟動、管理者手動啟動）時：

1. 檢查 `<根>` 的 ACL（第 4.5 節）；清除 `slots\.tmp-*`。
2. 讀狀態紀錄（第 4.3 節）。啟動器只讀 `state\`、`slots\`、`seed\`、`upgrade\work\`；**不讀 `request.json` 的內容**（只看它是否存在）、不讀 `%ProgramData%` 下的任何檔案來做決定。
3. **本機安裝的偵測**：目前啟動器的 SHA-256 與最新紀錄的 `launcher_sha256` 不同，或 `seed.json` 的雜湊與 `seed_seen_sha256` 不同 → 依第 4.8 節的規則處理（可能取消進行中的 OTA、可能開始種子試用）。
4. 有試用：`starts` 已達上限（建議 2）或累計確認時間已超過期限 → 放棄試用；否則寫 `trial_restart` 後啟動試用槽並繼續確認。
5. 沒有試用：啟動 `active`。
6. 以子行程啟動 `<槽>\pulse.exe agent run --workdir <根>`，放進設定了 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的工作物件。
7. 收到服務的停止或關機控制：通知子行程優雅結束（建議以繼承的事件控制代碼或關閉 stdin 管道；session 0 下 `GenerateConsoleCtrlEvent` 是否可用待驗證），逾時則結束工作物件，回報 `SERVICE_STOPPED`。試用中被停止時先把累計時間寫回。

**子行程結束時的處理（取代原設計「啟動器以相同結束代碼結束，交給服務控制管理員」；審查 2.1，採納）**：

| 情況 | 啟動器的動作 |
|---|---|
| 試用中，子行程以任何原因結束 | 判定不健康 → 放棄試用（第 4.7 節） |
| 非試用，結束代碼 2／3／4（用法、設定、憑證錯誤） | 啟動器以相同的服務特定結束代碼停止服務，**不重新啟動**（與 Linux unit 對這些代碼不重啟一致；`sc.exe query Pulse` 仍看得到代碼） |
| 非試用，其他結束（當機、結束代碼 1） | 啟動器**自己**重新啟動子行程，延遲依序 10 秒、60 秒、之後每 300 秒；子行程連續執行滿 1 小時後延遲歸零。服務維持 `RUNNING`，`pulse status` 顯示「代理程式重新啟動中」 |

服務控制管理員的失敗動作只負責**啟動器本身**當掉的情況：建議設為 `Restart`（10 秒）、`Restart`（60 秒）、`Restart`（300 秒），重設期 1 天，**不啟用**「非 0 結束代碼也算失敗」。依 X1 的事實，第四次以後會每 300 秒重複最後一個動作——這是刻意的：監控代理程式應該持續嘗試恢復，但頻率有上限。這樣做同時避開 X3（旗標要到重開機才生效）。「次數有上限、之後停止」（`Restart ×3, None`）是另一個選項，列為第 8 節 U13。以上行為全部待第 6 節實測。

其他命令列（`pulse status`、`pulse enroll`…）：啟動器把參數原樣轉送給 `active` 槽的 `pulse.exe`（繼承標準輸入輸出、傳回結束代碼）。`pulse --version` 因此顯示**執行中的代理程式版本**；`pulse launcher status` 顯示啟動器版本、`launcher_abi`、種子版本、`active`／`previous`／試用狀態、最近的狀態紀錄與任何高嚴重度事件。文件、CLI 與主控台都應同時呈現啟動器與代理程式兩個版本（審查對 U3 的意見，採納）。

### 4.5 信任鏈、簽章驗證點與 ACL

**沿用 `SHA256SUMS`＋`SHA256SUMS.sig`＋本機 `allowed_signers`，規則與 `REMOTE.md` 第 4.2.2 節相同，不新增信任來源。**

| 驗證點 | 誰 | 驗證什麼 |
|---|---|---|
| V1 下載前 | 代理程式 | 簽章、清單與發佈檔在 `SHA256SUMS` 中的雜湊、檔名、平台、版本政策（既有的 `on_offer`） |
| V2 下載後 | 代理程式 | 實際檔案的 SHA-256（既有的 `stage`） |
| V3 建槽前 | 輔助程式 | 把 `staging\` 的檔案複製到 `work\` 後全部重新驗證（含 `valid-after`／`valid-before`；信任錨與 `[upgrade]` 只讀 `<根>\pulse.toml`）。對應 ADR-0003 INV-3「在取得時驗證」 |
| V4 建槽後 | 輔助程式 | 解出的 `pulse.exe` 預檢：`--version` 等於目標版本、`check-config --json --with-remote` 通過 |
| V5 啟動前（每次，含切回） | 啟動器 | `slot.json` 完整；**槽內每個檔案**的 SHA-256 與 `slot.json` 的清單相符，且沒有清單以外的檔案（對應 INV-2；不是簽章驗證） |

- **V5 是完整性檢查，不是安全邊界**：能改槽內檔案的人也能改 `slot.json`。它偵測寫到一半的檔案與事後的損毀；防竄改靠目錄的 ACL，以及 E3 之後的權限隔離。
- **回滾當下不重新驗證簽章**（ADR-0003 INV-7、Q1）。**金鑰被顯式移除時**（不是 `valid-before` 到期）：輔助程式在每次升級的準備階段與 `pulse launcher status` 時，比對 `active`、`previous` 槽的 `record.json` 所記的簽署金鑰指紋是否仍列在 `allowed_signers`；不在時寫一筆狀態紀錄把該槽列入 `revoked`，視為**高嚴重度狀態**：記 `ERROR`、在 `pulse status`／`pulse launcher status` 顯示、經 `upgrade_status` 的 `detail`（或 ADR-0003 定義的欄位）回報給 Kairos。對後續升級的影響：`active` 被撤銷時仍然執行（拒絕它不會移除任何程式，只會讓主機沒有代理程式），但下一次提交成功後該槽**立即刪除、不保留為 `previous`**；`previous` 被撤銷時立即刪除，不再是手動回滾的目標。確切的欄位、代碼與是否限制後續提議，**以 ADR-0003 修訂後的條文為準**。
- **解壓縮的限制**（審查對 U7 的意見，採納）：只接受清單中列出的檔名；拒絕絕對路徑、`..`、磁碟代號、重複的名稱、僅大小寫不同的名稱、符號連結項目；單一檔案與總未壓縮大小有上限（建議總計 256 MiB）；壓縮比異常時拒絕。
- **信任錨的位置與遷移**（審查意見，採納；這是預設值的破壞性變更）：
  - 槽式安裝的預設改為 `<根>\allowed_signers`；`upgrade.allowed_signers` 有設定時以它為準。兩者都要通過下列 ACL 檢查。
  - **不自動沿用舊位置**。`%ProgramData%\Pulse\allowed_signers` 存在而新位置沒有時，`install.upgrade = "no_trust_anchor"`，本機訊息指示執行 `pulse upgrade trust --install <檔案>`：以系統管理員執行，顯示檔案中每把金鑰的指紋並要求確認，檢查來源檔的 ACL（寬鬆時警告但仍可由管理者確認），複製到新位置並設定只允許 SYSTEM、Administrators 的 ACL。
  - 影響面的評估：Windows 的遠端升級從未出貨（`install.upgrade` 一律是 `unsupported`），`REMOTE.md` 第 4.10 節提到的 MSI 屬性 `ALLOWEDSIGNERS=` 也沒有實作（wxs 中沒有），所以舊位置的檔案只可能是管理者依文件手動放的；它從未在 Windows 上被用來接受升級。CHANGELOG 仍須列為破壞性變更。
  - `default_allowed_signers_path()` 在 Windows 的回傳值、`REMOTE.md` 第 4.2.1 節與第 5.6 節的設定說明同步修改。
- **ACL 檢查**（輔助程式與啟動器都做；`file_perms.rs` 要從「警告」提升為「拒絕」，並補上目前沒有的擁有者、重新解析點與有效權限判斷）。對 `<根>`、`state\`、`slots\`、`seed\`、`upgrade\work\`、信任錨檔案，以及從磁碟區根目錄到 `<根>` 的每一層目錄：
  1. 擁有者是 SYSTEM、Administrators 或 TrustedInstaller。
  2. DACL 不是 NULL；沒有任何允許項目（含繼承的）把寫入類權限給上述三者以外的主體；拒絕項目不納入放行的理由（只看允許項目會多拒絕、不會漏放）。
  3. 不是重新解析點；開檔使用 `FILE_FLAG_OPEN_REPARSE_POINT`，並以控制代碼取得最終路徑比對，之後只用該控制代碼。
  4. 一般檔案才接受；硬連結數為 1。
- 不符合時 `install.upgrade` 不是 `ready`（第 4.11 節），啟動器仍照常啟動 `active`。
- **Authenticode**：不作為升級的信任條件（信任錨只有一個、與 Linux 一致、可離線驗證；作業系統憑證存放區中任何受信任 CA 簽發的憑證都不應成為升級的授權來源）。它對「不被防毒與應用程式控制阻擋」可能是必要的，屬於營運問題，見第 8 節 U4。

### 4.6 安全承諾的範圍與權限隔離

（審查 2.4 與 ADR-0003 第 7.3 節第 2 點，採納。）

**預覽階段（代理程式、輔助程式、啟動器同為 LocalSystem）承諾與不承諾的範圍：**

| 情況 | 預覽階段 | 進入「支援」（E3 完成後） |
|---|---|---|
| 新版啟動即結束、無法連線、不健康 | 自動切回 `active`（條件：啟動器與 `active` 槽完好） | 相同 |
| 升級過程中服務被停止、行程被結束、主機重開機 | 依第 4.3 節收斂（待第 6 節驗證） | 相同 |
| 斷電 | 依第 4.3 節收斂，**前提是儲存裝置遵守排清**（待第 6.2 節驗證） | 相同 |
| 被入侵的 Kairos 推送非官方或較舊的內容 | 簽章與降版政策擋下（與 Linux 相同） | 相同 |
| 一般本機使用者竄改槽、狀態紀錄、信任錨、暫存 | ACL 擋下（條件：`<根>` 的 ACL 符合第 4.5 節；MSI 安裝在 `%ProgramFiles%` 時預期符合，待實測） | 相同 |
| **代理程式（含試用中的新版）被入侵，或有嚴重的檔案操作錯誤** | **不承諾**。它是 LocalSystem，可以改寫狀態紀錄、槽、信任錨與啟動器；V5 與 ACL 對它沒有意義。ADR-0003 的 INV-1、INV-6、INV-11 在這個階段只是縱深防禦 | 承諾保住啟動器、狀態紀錄、`active`／`previous` 槽與信任錨 |
| 啟動器本身損毀、被防毒隔離、被應用程式控制阻擋，或 MSI 更新中斷 | 不承諾（服務無法啟動，需人工處理） | 不承諾 |
| 磁碟故障、檔案系統損毀 | 不承諾 | 不承諾 |

即使在預覽階段，下列規則也照 ADR-0003 的不變條件實作，E3 時不必重新設計：

- 啟動器只信任自己寫的狀態紀錄與特權目錄中的內容；切回的目標只來自狀態紀錄的 `active`（INV-1）。
- `request.json` 對啟動器只是「有請求」的訊號；`health.json` 只能讓啟動器**提交它自己選定的那個試用槽**，或不提交——不能改變目標（INV-11）。註：被入侵的試用版可以偽造健康訊號讓自己被提交；這與「惡意但行為正常的官方簽章版本」是同一種威脅，防線是簽章而不是健康確認。
- 輔助程式對複本操作、全部重新驗證、不採用 `request.json` 中會改變動作的欄位。

**E3：代理程式以低權限執行（進入「支援」的門檻）。** 目標：代理程式對 `<根>\pulse.exe`、`seed\`、`state\`、`slots\`、`upgrade\work\`、`allowed_signers`、`pulse.toml` 沒有寫入權限；只對 `buffer\`、`logs\`、`identity\`（待評估）、`upgrade\staging\`、`upgrade\request.json`、`upgrade\health.json` 有寫入權限。輔助程式仍由啟動器以 LocalSystem 啟動（它執行的是特權目錄中的 `active` 槽）。候選做法：

| 做法 | 說明 | 主要風險 |
|---|---|---|
| (a) 啟動器以服務帳號的權杖啟動子行程 | 啟動器（LocalSystem）取得 `NT AUTHORITY\LocalService` 或專用帳號的權杖後 `CreateProcessAsUser`；資料目錄對該帳號授權 | 從 LocalSystem 取得該權杖的方式與所需特權待驗證；蒐集功能（磁碟 I/O、行程清單與命令列、SMBIOS）在低權限下是否可用待驗證（`CHANGELOG.md` [0.8.0] 已列為未驗證） |
| (b) 啟動器以受限權杖啟動子行程 | `CreateRestrictedToken`：停用 Administrators 群組、移除大部分特權、加上限制用 SID，只有授權給該 SID 的目錄可寫 | 仍是 SYSTEM 身分，隔離強度與相容性都需要仔細驗證 |
| (c) 兩個服務（方案 D） | 代理程式是另一個以虛擬服務帳號執行的服務，由特權服務控制其啟停與執行檔路徑 | 多一個服務的安裝、啟停與失敗復原；執行檔路徑要指向槽 |

建議在階段 0／1 以小型實驗比較 (a) 與 (b)，選定後另寫設計（可作為本 ADR 的補充或新的 ADR）。這項工作**另外估計**，不在第 7 節的工程人週內。若使用者決定不做 E3，則「支援」的文件必須明列上表的「不承諾」，且不得宣稱 ADR-0003 的 INV-6 在 Windows 是安全邊界（第 8 節 U14）。

### 4.7 健康確認與自動切回

條件沿用 `REMOTE.md` 第 4.5.1 節與第 17.2 節第 12 項，執行者由輔助程式改為啟動器。**這不只是路徑不同：健康訊號的來源與信任邊界都改變了**（審查意見，採納），需寫進 `REMOTE.md` 第 4.5 節：

| 項目 | `REMOTE.md` 現行（Linux；原規劃的 Windows） | Windows 槽式 |
|---|---|---|
| 行程是否存活 | systemd `ActiveState`／Windows 服務 `RUNNING`、`NRestarts` | 啟動器直接持有子行程的控制代碼；試用期間任何一次結束即不健康 |
| 健康訊號 | `status.json`（Windows 在 `%ProgramData%\Pulse`） | `<根>\upgrade\health.json`（`pid`、版本、`connection.state`、`reporting.last_ack_at`）；啟動器確認 `pid` 是自己啟動的子行程、版本等於試用槽的版本。不讀 `%ProgramData%` |
| 穩定期 | 60 秒 | 相同 |
| **`health_timeout_secs` 的語意** | 替換後的期限（牆上時間） | **試用槽累計執行的確認時間**（不含停機與重開機的時間），另以啟動次數上限（建議 2）與牆上時間的硬上限（自 `trial_start` 起 7 天）限制 |

`health_timeout_secs` 的語意變更對 Kairos 的影響：`slot` 主機的終止狀態可能比 `health_timeout_secs` 晚（中間的停機不計入），Kairos「推送中的主機超過期限未回報」（第 4.7.4 節）的判斷與主控台的預期完成時間要把這一點寫明；協定欄位本身不變。

- **通過**：寫 `commit`、`result.json`（`succeeded`），清除暫存與舊的 `previous`。
- **不通過、逾時、啟動次數超過上限、進度檔遺失**：放棄試用（第 4.3 節），以相同條件、`max(health_timeout_secs, 300)` 的期限確認 `active` 恢復 → `rolled_back`（`PLS-W8407`）。
- **`active` 也不健康**：`rollback_failed`（`PLS-E8408`）。與 Linux 的差別：啟動器仍是服務本體，會依第 4.4 節的退避規則繼續嘗試啟動 `active`（它是最後確認過的版本），但不再接受任何升級，直到管理者處理或收到新的提議。
- 「連上 Kairos 並有一筆 report 被確認」仍是健康條件的一部分（第 4.5.2 節的保守選擇不變）。

### 4.8 種子、既有安裝的遷移，以及 MSI 與 OTA 同時發生時的收斂規則

**種子規則**（審查 2.3 與 ADR-0003 第 7.3 節第 3 點，採納）：

- **完全沒有有效的狀態紀錄**（第一次安裝，或紀錄全毀）：以種子建立初始槽，直接成為 `active`（沒有別的版本可以退）。
- **已經有 `active`**：種子的版本**高於** `active` 的版本，且這份種子（以雜湊辨識）沒有被處理過 → **走與 OTA 完全相同的試用、健康確認與自動切回**，`trial.source = "seed"`。通過才提交；不通過則切回原本的 `active`，並把這份種子的雜湊記為已處理（同一份種子不會在每次啟動時重試；換了新的種子才會再試）。結果以 `pulse launcher status` 與下一次連線的事件回報。
- 種子的版本低於或等於 `active`：不建槽，只更新 `seed_seen_sha256`。
- 種子槽的信任來源是「本機管理者的安裝行為」（INV-8），`slot.json` 記 `source: "seed"`，可以當回滾目標（ADR-0003 Q2）。

**既有安裝的遷移**：

| 情境 | 行為 |
|---|---|
| 全新安裝（MSI 或 zip） | 啟動器第一次啟動時以種子建立初始槽 |
| 0.8／0.9／0.10 的 MSI 安裝 → 含啟動器的版本 | 安裝新版 MSI。服務的命令列不變；沒有狀態紀錄 → 初始槽。`pulse.toml`、`agent.id`、`identity\`、`buffer\`、`logs\` 原地沿用。**注意 X4**：MSI 升級後服務是否自動啟動待驗證 |
| 0.8／0.9／0.10 的 zip 安裝 | 停止服務、解開新版（啟動器與 `seed\`）、啟動服務。`<根>` 的 ACL 不符合第 4.5 節時 `install.upgrade = "insecure_path"`，其餘照常 |

每台 Windows 主機都要手動升級一次到含啟動器的版本，之後才能從 Kairos 推送（與 `REMOTE.md` 第 4.10 節的 Linux 相同）。

**MSI／zip 的本機安裝與 OTA 同時發生時的收斂規則**（審查 2.5，採納）。沒有自訂動作的 MSI 無法取得我們的鎖，它對啟動器可見的效果只有三種：服務被停止、`<根>\pulse.exe` 被換掉、`seed\` 被換掉。因此收斂規則建立在啟動器啟動時的判斷上，不依賴跨程序的鎖：

- **R1（本機安裝優先）**：啟動器啟動時發現自己的雜湊或種子的雜湊與最新紀錄所記的不同，而且有進行中的 OTA（建槽中、已封存未切換、試用中）→ 寫 `supersede` 取消該 OTA：刪除試用槽與暫存，`result.json` 記 `failed`、`detail: superseded_by_local_install`，之後回報給 Kairos。然後才套用種子規則。理由：本機管理者的動作是明確的意圖，而且新的啟動器不必理解舊啟動器進行到一半的試用。
- **R2（服務被停止但啟動器與種子都沒有變）**：視為一般的停止／重開機，依第 4.3 節接續。
- **R3（輔助程式執行中被停止）**：工作物件結束輔助程式；下次啟動清除 `.tmp-*`，`request.json` 還在則重新建槽（除非 R1 成立）。
- **R4（啟動器與輔助程式之間）**：啟動器在「建槽 → 切換 → 提交／切回」期間持有 `<根>\upgrade\lock`（獨占開啟）；`pulse upgrade rollback`、`pulse upgrade trust` 等本機命令取不到鎖時回報「升級進行中」並結束。

預期結果（第 6 節 S7 逐項斷言）。記號：`I` 為目前安裝的 MSI 版本，`M` 為要安裝的 MSI 版本，`A` 為 `active` 的版本：

| # | 情境 | 預期結果 |
|---|---|---|
| M1 | `M > I` 且 `M > A`，沒有進行中的 OTA | 啟動器與種子換成 `M`；種子試用；健康 → `active = M`、`previous = A`。不健康 → 切回 `A`，「應用程式與功能」顯示 `M`、代理程式為 `A`，`pulse launcher status` 與主控台顯示兩個版本不同 |
| M2 | `M > I` 但 `M ≤ A`（OTA 已經超前） | 啟動器與種子換成 `M`；`active` 不變、不降版 |
| M3 | `M < I` | Windows Installer 以 `DowngradeErrorMessage` 拒絕；服務、狀態紀錄、槽都不變（拒絕是否發生在停止服務之前，待驗證；斷言：`msiexec` 以失敗結束、服務仍是 `RUNNING`） |
| M4 | `M` 與 `I` 的數字版本相同、預發佈標記不同（`ProductCode` 相同、`PackageCode` 不同） | 預期 Windows Installer 拒絕（「另一個版本已安裝」，待驗證）；狀態不變。**測試用的 MSI 因此必須使用不同的數字版本**（不能只靠 `-test.N` 區分） |
| M5 | 建槽中安裝 `M`（`M > I`） | R3＋R1：OTA 取消並回報 `failed`；之後依 M1 或 M2 |
| M6 | 試用中安裝 `M`（`M > I`） | R1：試用取消（不提交、刪除試用槽）；之後依 M1 或 M2。不論 `M` 與試用版本孰高都一樣——Kairos 之後會依主機回報的版本重新提議 |
| M7 | `msiexec /fa` 修復 | 啟動器與種子的內容不變（雜湊相同）→ R2；服務被停止後是否自動啟動見 X4 |
| M8 | 解除安裝後重新安裝（`slots\`、`state\` 留在磁碟上） | **依殘留的狀態紀錄繼續**：`A ≥ M` 時仍執行 `A`。這與「解除安裝後重裝會得到 MSI 的版本」的直覺不同，沒有自訂動作時無法區分「升級」與「移除後重裝」。INSTALL 的移除步驟要求刪除 `slots\`、`state\`、`upgrade\`；另提供 `pulse launcher reset --to-seed`（系統管理員；以種子重建初始槽） |
| M9 | 新啟動器的 `launcher_abi` 無法執行 `active`（只在管理者刻意安裝更舊的啟動器時發生；MSI 路徑會被 M3 擋下，zip 路徑不會） | 依序嘗試 `previous`；都不相容時以種子重建初始槽並記高嚴重度事件（等同管理者選擇降版） |
| M10 | 舊啟動器遇到較新格式的狀態紀錄 | 只依信封啟動 `active`，不寫任何紀錄，`install.upgrade = "unsupported"`（本機原因：啟動器過舊） |

MSI 解除安裝：停止並移除服務、刪除 MSI 管理的檔案（啟動器、種子）；`slots\`、`state\`、`upgrade\` 與 `pulse.toml`、身分一樣留在磁碟上（wixl 的 `RemoveFolderEx` 支援待驗證）。

### 4.9 啟動器與槽格式的相容矩陣

（審查遺漏項，採納。）

| 情況 | 規則 |
|---|---|
| 新啟動器、舊格式的狀態紀錄或 `slot.json` | 必須能讀取所有已發佈過的格式；第一次寫入時以新格式寫新的 generation（舊紀錄保留到被淘汰） |
| 新啟動器接手舊啟動器進行中的試用 | 不接手：R1 取消 |
| 舊啟動器、新格式的狀態紀錄 | 只依信封（`envelope`、`generation`、`active`、`checksum`）啟動 `active`；不寫入、不升級（M10） |
| 代理程式要求的 `launcher_abi` 高於啟動器 | 發佈清單的 `min_launcher_abi` 高於本機啟動器 → `declined`（沿用 `min_upgrade_from` 的處理），需先以 MSI 更新啟動器 |
| 啟動器支援的最低代理程式 ABI 高於 `active` | M9 |
| MSI 更新中斷（啟動器或種子只換了一半） | 啟動器檔案損毀 → 服務無法啟動，需人工（不承諾，第 4.6 節）；種子與 `seed.json` 的雜湊不符 → 忽略種子、照常啟動 `active`、記 `ERROR` |

信封欄位永不改變意義；`launcher_abi` 只增不減，啟動器宣告自己支援的範圍 `[min, max]`。

### 4.10 與 MSI 安裝紀錄的關係

- OTA 之後，「應用程式與功能」與 `ProductCode` 顯示的是最後一次以 MSI 安裝的版本（啟動器與種子的版本），不是執行中的代理程式版本。執行中的版本以 Kairos 主控台、`pulse --version`、`pulse status` 為準。
- 這改變 `REMOTE.md` 第 18.3 節第 2 項的通過條件（原本要求三者一致）與第 12 項（SCCM 偵測）。以「產品碼存在」或「`pulse.exe` 存在」為偵測規則的部署不受影響；以「版本 ≥ X」為規則的部署會重新部署 MSI，結果依 M1／M2。
- 不改寫解除安裝登錄機碼的 `DisplayVersion`（審查同意）：它只改顯示，Windows Installer 自己的資料庫不變。

### 4.11 與協定的關係

- **協定版本不升、不新增 feature**；`upgrade` feature 的訊息完全沿用。
- **新的安裝方式值 `install.method = "slot"`**：
  - 已確認的相容性：`method` 是 1～32 bytes 的小寫字串，不認得的值由 Kairos 原樣保存；`release::artifact_kind` 對不認得的值回 `None`，`rollout.rs` 標為 `skipped`（`unsupported_platform`）。0.9／0.10 的 Kairos 遇到 `slot` 的主機不會提議升級。
  - 新版 Kairos：`slot`＋Windows x86_64 → `pulse-<版本>-<commit>-windows-x86_64.zip`；資格判斷改為「非 Linux 只接受 `slot`」。不送 `rollback_artifact`、`rollback_bundle`（ADR-0003 Q4）。
  - `msi`、`zip` 兩個值保留給舊式配置，維持不支援。
- **`install.upgrade` 的值**：

  | 情況 | 值 |
  |---|---|
  | 槽式配置、信任錨與 ACL 都符合、`upgrade.enabled`、建置允許（第 7 節） | `ready` |
  | 舊式配置（沒有啟動器） | `no_helper` |
  | `<根>` 或其祖先的 ACL 不符 | 新增 `insecure_path`（舊版 Kairos 分類為 `not_ready`） |
  | 信任錨不存在、只在舊位置、或 ACL 不符 | `no_trust_anchor` |
  | 啟動器過舊（M10）、不在本機固定磁碟 | `unsupported` |

- **hello 的 `install` 物件加選用欄位** `launcher`（啟動器版本）與 `launcher_abi`；第 2.1 節已規定「其他欄位忽略」。
- **發佈清單**加選用欄位 `min_launcher_abi`（清單與 `UpgradeOffer` 的解析沒有 `deny_unknown_fields`，舊版忽略）。
- **規格同步**（審查次要問題，採納）：`REMOTE.md` 第 2.1 節目前只列六種 `method` 與五種 `upgrade` 狀態，核定後必須連同 `PROTOCOL.md` 與 `kairos-proto` 的常數、測試向量一起更新；`health_timeout_secs` 的語意差異寫進第 2.4.1 節與第 4.5 節。
- **代碼**：新情況需要的代碼由設計包正式分配；本 ADR 不暫定號碼。

### 4.12 與 ADR-0003 的關係

本文採用 ADR-0003 第 7.1 節對 Q1～Q6 的結論、第 7.2 節的條款與第 7.3 節的五點建議：

| ADR-0003 | 本文的落實 |
|---|---|
| Q1／INV-7：回滾當下不重驗簽章，只檢查完整性；啟動器不含簽章驗證 | 第 4.1、4.5 節（V5）；金鑰顯式撤銷的處理見第 4.5 節 |
| Q2／INV-8：種子槽可以當回滾目標 | 第 4.8 節 |
| Q3：本機紀錄統一內容格式（`record.json`），鍵為 (版本, commit, 發佈檔 SHA-256)，另記執行檔雜湊 | 第 4.2 節 `slots\<槽>\release\`；槽 ID 也含發佈檔雜湊 |
| Q4：`rollback_bundle` 只對 deb／rpm | 第 4.11 節 |
| Q5／INV-9：第一版不提供遠端「切回前一槽」 | 不提供；日後若做是新的白名單動作 |
| Q6：`previous` 保留到下一次提交；手動回滾不受遠端降版政策限制但要過完整性檢查 | 第 4.2、4.3 節（`manual_rollback`） |
| 7.3-1：掃描退路不得選「版本最高」 | **取消掃描**；以 generation 最大的有效紀錄為準（第 4.3 節）。ADR-0003 建議的 `confirmed_at` 是牆上時間，改用單調遞增的 generation（審查 2.2 的意見） |
| 7.3-2：Windows 沒有特權分離 | 第 4.6 節：縮小承諾、E3 為支援門檻、啟動器不讀代理程式可寫的檔案來決定目標 |
| 7.3-3：種子切換也走試用 | 第 4.8 節 |
| 7.3-4：Windows 的 `release\` 對應 Linux **第二階段的 root 封存**，不是服務帳號可寫的 `upgrade/releases/` | 採用此說法。注意在 E3 之前，Windows 的這份紀錄仍可被 LocalSystem 的代理程式改寫，信任等級只在「擋一般使用者」的意義上成立 |
| 7.3-5／INV-10：失敗的版本不得被自動選中 | 失敗的試用槽不保留執行檔、不會出現在任何紀錄的 `active`／`previous`（第 4.2、4.3 節） |

尚待 ADR-0003 修訂後定稿的一點：金鑰顯式撤銷的「高嚴重度狀態」如何回報給 Kairos（欄位與代碼）、是否限制後續提議。本文第 4.5 節先寫出 Windows 端的行為，以 ADR-0003 的條文為準。

### 4.13 啟動器自身的升級

- 啟動器與種子只由 MSI／zip 更新。OTA 不改寫 `<根>\pulse.exe` 與 `seed\`，因此不碰 MSI 管理的檔案。
- 啟動器有安全修正而必須更新時：發佈清單提高 `min_launcher_abi`，主控台依 hello 的 `launcher`／`launcher_abi` 列出需要以 MSI 更新的主機。
- 保留的演進路徑（不在第一版）：啟動器也做成槽（需要更小的第零層或改寫服務的 `ImagePath`）。

---

## 5. 後果

### 5.1 正面

- **切回不需要任何檔案操作**：`active` 在提交前不會被改動；新版不健康、升級中服務被停止或主機重開機之後，啟動器啟動最後確認過的版本。**條件**：啟動器、`active` 槽與至少一筆有效的狀態紀錄完好；斷電的情況另以儲存裝置遵守排清為前提（待第 6.2 節驗證）。
- **不需要 `rollback_artifact`**：Windows 避開 `REMOTE.md` 第 17.3 節第 1 項的信任問題與「套件資料庫與執行檔不一致」。
- **升級路徑不經 Windows Installer**：1618、3010、交易回復、`AllowDowngrades` 都不在升級路徑上（MSI 與 OTA 的互動另以第 4.8 節的規則處理）。
- **信任邊界落在預期預設就安全的位置**（MSI 安裝時；確切的預設 ACL 待實測），自訂 ACL 的程式是縱深防禦。
- **可驗證性高**：每一步都是自己的程式與檔案操作，可以在 CI 的 Windows runner 上實際執行並注入故障。
- 協定與 Kairos 端的改動小。

### 5.2 負面

- **MSI 安裝紀錄的版本與執行中的版本不一致**；解除安裝後殘留的槽會影響重新安裝的結果（M8）。
- **多一個只能靠 MSI 修正的單點**（啟動器）；行程樹變成兩層，`pulse status`、`pulse diag`、文件與支援流程都要改。
- **預覽階段沒有權限隔離**：代理程式被入侵或嚴重出錯時，A/B 不提供保護（第 4.6 節）。
- **工作量明顯高於原估**（第 7 節）。
- **所有既有的 Windows 安裝都要手動升級一次**，而且這一次改變了檔案配置。
- 磁碟用量：平時 `3E`、峰值 `4E + 2Z`（第 4.2 節）。
- 每次升級都有一個新路徑的、未簽章的執行檔第一次被執行（防毒與應用程式控制的反應待驗證）。
- `health_timeout_secs` 在 Windows 的語意與 Linux 不同（第 4.7 節）。
- 信任錨預設位置改變是破壞性變更。

### 5.3 殘餘風險

| 風險 | 說明 | 緩解 |
|---|---|---|
| 啟動器的缺陷或損毀 | 選槽或切回的邏輯錯誤、檔案被隔離或阻擋時服務起不來，OTA 修不了 | 獨立的小型執行檔、性質測試與故障注入；`launcher_abi`；試點（第 7 節） |
| 代理程式被入侵（預覽階段） | LocalSystem 可改寫一切 | 明列為不承諾；E3 為支援門檻 |
| 斷電時的持久性 | 排清後的紀錄仍可能遺失或損毀 | 追加式紀錄、回溯到更舊的 generation、最後退到種子；斷電測試 |
| 共用資料的格式 | 新版寫了舊版讀不懂的資料，切回後舊版無法啟動 | 與 Linux 相同；格式只增不改、`min_upgrade_from`；回滾情境以「新版先執行一段時間再切回」驗證 |
| ACL 檢查的漏洞 | zip 安裝在寬鬆目錄的主機可能有提權路徑 | MSI 安裝預期不依賴它；CI 與 VM 以實際的非特權帳號攻擊 |
| 防毒／應用程式控制 | 新槽被隔離或阻擋 | V5 在切換前檢查；切換後被阻擋等同啟動失敗 → 切回。啟動器本身被阻擋則無解 |
| 健康條件依賴 Kairos | 切換期間 Kairos 停機會造成不必要的切回 | 既有的保守選擇 |
| 沒有涵蓋的環境 | 網域原則、第三方 EDR、Server Core、非英文系統、ReFS、Windows Server 2016 以前 | 支援矩陣只列驗證過的；未驗證的明確排除 |
| X1～X4 | 既有的服務失敗復原與 MSI 行為與文件不符 | 階段 0 實測並修正 |

---

## 6. 驗證計畫

原則不變：**沒有在 Windows 上實際執行過的行為不出貨。** 分三層，前兩層不需要使用者先購買設備。

### 6.1 第一層：GitHub Actions 的 Windows runner

依 GitHub 的文件，GitHub 代管的 Windows runner「configured to run as administrators with User Account Control (UAC) disabled」，目前提供 `windows-2022`、`windows-2025`（`windows-latest` 對應 2025）（[GitHub-hosted runners](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)）。現有的 `kp-e2e` 已在 `windows-latest` 上啟動實際的 `kairos` 與 `pulse`。

新增 job `windows-system`（`needs: package`；矩陣 `windows-2022`、`windows-2025`），使用 `package` job 產出的、**實際要發佈的** MSI 與 zip，以測試金鑰簽署，腳本放在 `tests\windows\`。測試建置必須使用**不同的數字版本**（M4）。

| 群組 | 情境與斷言 |
|---|---|
| S0 基礎（補 0.8.0 的未驗證項目與 X1～X4） | `msiexec /i … /qn` 安裝、服務的命令列與帳號、`pulse enroll` 後啟動、工作目錄、`status.json`、停止與重新啟動、MajorUpgrade 後服務的狀態（X4）、`msiexec /x` 後留下的檔案、zip＋`pulse service install --start`、各目錄的實際 ACL（`icacls` 輸出存成 artifact）；**失敗復原**：連續結束服務行程 5 次以上，記錄第 4 次以後是否仍重新啟動（X1）、非 0 結束代碼在重開機前後是否觸發（X3，重開機的部分在 VM）、`sc.exe qfailure` 的完整輸出 |
| S1 槽與啟動器 | 初始槽、`pulse --version` 的轉送與結束代碼、服務停止時子行程一起結束、子行程結束代碼 2／3／4 → 服務停止且不重啟、當機 → 啟動器依 10／60／300 秒退避重啟、啟動器被結束 → 服務控制管理員重啟 |
| S2 升級成功 | 以實際的 Kairos 推送：建槽期間舊版持續回報、切換、健康確認、`commit`、`succeeded`、暫存清除、`previous` 保留、狀態紀錄的 generation 連續 |
| S3 自動切回 | 「啟動即結束」與「啟動但連不上」兩種建置：`rolled_back`、試用槽已刪除、`upgrade\failed\` 有紀錄、推送自動暫停；之後同一個版本與 commit 重試（槽 ID 不衝突）、同版重新簽署、兩個推送指向相同內容 |
| S4 故障注入 | （a）第 4.3 節表中的**每一個當機點**：`taskkill /F` 整個行程樹後重新啟動服務，斷言結果與表一致。（b）檔案層：最新一筆紀錄截斷、`checksum` 錯誤、最新 3 筆全毀、`state\` 清空、`slot.json` 遺失、槽內檔案被改、槽內多出檔案、`trial-progress.json` 損毀。（c）資源層：磁碟滿（以配額或小型 VHD）、目錄唯讀、`state\` 寫入被拒、改名失敗 |
| S5 信任與 ACL | 不信任的金鑰（`PLS-E8401`）。以 `net user` 建立**不屬於 Administrators 的本機帳號**，以該帳號執行：在 `state\`、`slots\`、`upgrade\work\` 建立或替換檔案、建立 junction 與目錄掛接點、建立硬連結、利用 8.3 短檔名與大小寫差異、嘗試把祖先目錄改名換掉、預先建立 `%ProgramData%\Pulse` 並放入偽造的 `status.json`。ACL 判斷：繼承的允許項目、拒絕項目、擁有者為一般使用者、NULL DACL；zip 裝在 `C:\pulse-test` 時的判斷；信任錨只在舊位置時為 `no_trust_anchor`；`pulse upgrade trust --install` 的結果 ACL |
| S6 鎖檔 | 另一個行程以不共用的方式開啟舊槽的檔案、`staging\` 的檔案、最新的狀態紀錄：重試、可診斷的錯誤、不會卡住 |
| S7 與 MSI／zip 並用 | 第 4.8 節的 M1～M10 **逐項斷言**（服務狀態、`msiexec` 結束代碼、`active`／`previous`、狀態紀錄的 `reason`、回報給 Kairos 的結果）；另加 MSI 更新中斷（安裝中結束 `msiexec`） |
| S8 解壓縮 | 路徑穿越、重複名稱、超過大小上限、清單以外的檔案、壓縮比異常的 zip 都被拒絕 |

限制：runner 不能重開機、沒有真正的斷電、是 Server 版、沒有加入網域、每次都是全新的映像、Microsoft Defender 即時保護是否開啟未查證（以 `Get-MpComputerStatus` 記錄）。故障注入以「強制結束行程」近似當機，**不能證明檔案系統在斷電時的行為**。在 runner 上以另建的非特權帳號執行攻擊腳本是否可行，本身待驗證。

### 6.2 第二層：自管的 Windows VM

使用者既有的 PVE 上建立 Windows VM，使用 Microsoft 的評估版（授權條款與有效天數以 Microsoft 下載頁為準，待確認）。執行同一套 `tests\windows\`，另外加上：

| 項目 | 做法 |
|---|---|
| 正常重開機 | 在第 4.3 節的每個當機點重開機，斷言收斂 |
| 斷電 | PVE 的 `qm stop` 在每個當機點各做多次（建議每點 20 次），開機後斷言：服務啟動、執行的是表中預期的槽、狀態紀錄可讀或已依規則退回。**這是唯一能驗證第 4.3 節的容錯是否足夠的測試**；VM 的磁碟快取設定要記錄（不同設定的結果可能不同） |
| 既有安裝的遷移 | 「0.8.0／0.10.0 MSI 安裝並執行一段時間」的快照 → 新版 MSI |
| 啟動器 | 長時間執行（至少連續 7 天）、重開機、服務停止、子行程崩潰、MajorUpgrade |
| Defender 即時保護與雲端保護開啟 | 新槽、種子、啟動器是否被隔離；掃描造成的延遲與鎖檔 |
| 用戶端版 Windows | Windows 11 |
| X3 | 失敗復原旗標在重開機前後的行為 |
| 長時間循環 | 連續 20 次以上的升級／切回，檢查槽的清理與磁碟用量 |

### 6.3 第三層：真實環境或客戶試點

網域加入與 GPO、AppLocker／WDAC、SCCM 的部署與偵測規則、第三方防毒／EDR、Server Core、非英文語系、實體機的斷電。進入「支援」前至少驗證**一種實際的企業 EDR 或應用程式控制設定**，否則支援矩陣明確排除第三方 EDR、AppLocker、WDAC（審查遺漏項，採納）。

### 6.4 `REMOTE.md` 第 18.3 節 12 項清單的對應

| # | 原情境 | 在槽式設計下 | CI 可驗 | 需 VM | 備註 |
|---|---|---|:-:|:-:|---|
| 1 | MSI 安裝、放好 `allowed_signers` | 保留；`install` 改為 `slot`／`ready`；信任錨以 `pulse upgrade trust --install` 放入 | ✓（S0、S1、S5） | 用戶端版再做一次 | `ALLOWEDSIGNERS=` 視 wixl 的能力（U8） |
| 2 | 推送成功 | 保留；通過條件改寫為第 6.1 節 S2。**「應用程式與功能的版本一致」這一條移除** | ✓（S2） | 用戶端版再做一次 | |
| 3 | 啟動即結束 → 回滾 | 保留；切回 `active`，不需要 `rollback_artifact` 與 `AllowDowngrades` | ✓（S3） | | |
| 4 | zip 安裝的升級與回滾、鎖檔 | 與 MSI 走同一條路徑；`pulse.exe.prev` 不再存在 | ✓（S2、S3、S6） | 真實防毒的鎖檔 | |
| 5 | 確認期間重開機（正常關機、斷電） | 保留並擴大到每個當機點 | 部分（S4 只是近似） | **✓ 必須** | 決定能否進入預覽 |
| 6 | `msiexec` 1618 | 升級路徑不再適用。改為 MSI 與 OTA 同時發生（M5、M6） | ✓（S7） | | |
| 7 | `msiexec` 3010 | 升級路徑不再適用（槽式升級不需要重開機）；MSI 本身回 3010 的情況屬於 S0 | — | — | |
| 8 | ACL 與提權 | 保留並擴大 | ✓（S5；可行性待驗證） | ✓ 以互動登入的標準使用者與網域使用者重做 | runner 的預設帳號是系統管理員且 UAC 關閉，必須另建帳號 |
| 9 | 簽章錯誤 | 保留 | ✓（S5） | | |
| 10 | 防毒軟體 | 保留；對象改為新槽、種子與啟動器 | 僅在 runner 的 Defender 開啟時當作初步證據 | **✓ 必須** | U4 的依據 |
| 11 | 服務帳號與 session 0 | 剩下「LocalSystem、session 0 下啟動器能啟動並優雅停止子行程」 | ✓（S1） | 由 GPO／SCCM 在無人登入時部署 MSI | E3 之後要重做 |
| 12 | SCCM 並用（選做） | 保留；預期依第 4.10 節改寫 | 以腳本模擬偵測規則 | 真正的 SCCM 屬第三層 | |

---

## 7. 實作計畫

人週為**規劃範圍，不是承諾值**（審查意見，採納：原估 5.5～7 人週過於樂觀）。工程與自動測試、權限隔離、試點觀察分開列。

### 7.1 工程與自動測試（合計約 8～12 人週）

| 階段 | 內容 | 粗估 | 完成條件 | 對外狀態 |
|---|---|---|---|---|
| **0 基礎驗證** | `windows-system` job 與 S0；實測 X1～X4 並修正（失敗動作的完整比對與設定、文件更正）；記錄預設 ACL；確認 wixl 的能力（`ServiceControl Start`、權限、自訂動作、`RemoveFolderEx`）；E3 候選做法的小型實驗 | 1～1.5 人週 | S0 在兩個 runner 映像連續通過；X1～X4 有實測結論 | 不支援。X1～X4 的修正可隨一般版本出貨（屬於既有缺陷的修正，不含啟動器） |
| **1 啟動器與槽** | 獨立的啟動器 crate、狀態紀錄、種子與種子試用、子行程監看與退避、命令列轉送、`pulse launcher status`、相容矩陣；`install.method = "slot"`；S1、S4（與 OTA 無關的部分）、S7 的 M1～M4、M7～M10 | 2～3 人週 | 上述情境通過；狀態紀錄的讀取規則有性質測試 | **不單獨進正式發佈**（見下） |
| **2 升級** | 輔助程式的 Windows 版（ACL 檢查、zip 解開與限制、預檢、建槽封存）；啟動器的試用、健康確認、提交、切回；`health.json`；信任錨遷移命令；Kairos 的 `slot` 對應與資格；發佈清單 `min_launcher_abi`；撤銷狀態 | 2.5～3.5 人週 | S2、S3、S5、S6、S8 通過 | 不支援 |
| **3 故障注入與互動矩陣** | S4 完整矩陣、S7 的 M5、M6 與 MSI 中斷；CI 夜間執行；修正 | 1.5～2 人週 | S4、S7 在兩個映像連續 20 次全數通過 | 不支援 |
| **4 VM 驗證** | 第 6.2 節全部項目，於 Windows Server 2022 與 Windows 11 | 1～2 人週＋等待時間 | 第 6.4 節「需 VM」的項目全數通過；斷電測試每個當機點 20 次沒有任何一次需要人工處理 | **→ 預覽** |

**階段 1 不單獨先出貨**（審查 2.7，採納；原建議撤回）。啟動器是新的 LocalSystem 服務本體與單點，即使 OTA 關閉也一樣；只在 runner 上通過 S0／S1 不符合本文自己的實機原則。含啟動器的建置最早在階段 4 的 VM 驗證（既有安裝遷移、重開機、服務停止、子行程崩潰、Defender、MajorUpgrade、長時間執行）通過後，才進入**明確隔離的預覽通道**。

**預覽的定義**：

- 以編譯期開關與發佈通道隔離：正式發佈的建置在 Windows 上仍回報 `unsupported`；預覽建置另外產出、另外標示，不放進一般客戶的升級路徑。**不只依賴本機的布林設定**（審查對 U5 的意見，採納）。
- 預覽建置中 `upgrade.enabled` 在 Windows 預設 `false`。
- 文件明列：驗證過的 Windows 版本（只列實際在 VM 驗證過且仍在供應商支援期內的版本——依階段 4 是 Windows Server 2022 與 Windows 11；Windows Server 2025 若只在 CI 驗證，標示為「CI 驗證」而不列入）、第 4.6 節的「不承諾」、未驗證的環境。

### 7.2 權限隔離（E3）：另行估計

第 4.6 節。粗略的規劃範圍 3～5 人週，**不確定性高**（取決於階段 0 的實驗結果與蒐集功能在低權限下的相容性），需要自己的 VM 驗證（S5、第 11 項重做）。它是進入「支援」的門檻。

### 7.3 試點與進入「支援」：以曆時計，不計入人週

進入「支援」的條件（數字為建議值，第 8 節 U15）：

1. E3 完成並通過驗證（或使用者依 U14 明確決定不做並接受文件上的限制）。
2. 試點：至少 20 台主機（自管 VM 與客戶試點合計，至少兩種 Windows 版本）、至少兩個真實發佈、每個發佈之後觀察至少 4 週。
3. 結果：`rollback_failed` 為 0、需要人工復原的主機為 0、升級成功率 ≥ 99%（失敗而自動切回成功的不算人工復原，但要逐一分析）；期間至少刻意推送一次會失敗的版本，自動切回在所有試點主機上成功。
4. 至少一種實際的企業 EDR 或應用程式控制設定通過，否則支援矩陣明確排除。
5. U3、U4 定案；INSTALL、SCCM 文件、主控台說明完成；`CHANGELOG.md`「支援期限」改列驗證過的版本。

Authenticode（U4）與第三方 EDR 的驗證若列入，另行估計。

### 7.4 需要同步修改的文件（核定後）

`REMOTE.md` 第 2.1、2.4.1、4.2.1、4.3、4.4.2、4.4.4、4.5、4.6、4.10、5.6、12.4、18 節；`PROTOCOL.md`「遠端管理」；`kairos-proto` 的常數與測試向量；`pulse/packaging/INSTALL.md` A.3 與第 10.1 節（含移除步驟）；`pulse/README.md`；`RELEASING.md`；`CHANGELOG.md`（含 X1 的更正）；`pulse/src/winsvc.rs` 的註解與 `--help`；設計包 `pulse/sccm-deployment.md`、`pulse/cli-spec.md`；ADR-0002（交叉引用與統計口徑）。

---

## 8. 待決事項（需要使用者決定）

| # | 事項 | 選項 | 建議（修訂後） |
|---|---|---|---|
| U1 | 是否核定：預覽採 E1（槽＋獨立啟動器，MSI 為 bootstrap），「支援」以 E3 為門檻；放棄就地 `msiexec` 與 zip 就地替換 | 核定／改採其他方案 | **核定**（已納入審查的七項重大修改） |
| U2 | 驗證環境 | (a) GitHub Actions runner＋既有 PVE 上的評估版 Windows VM；(b) 購買或租用實體機；(c) 只用 CI | **(a)**。VM 是進入預覽的必要條件；CI 不能取代斷電與 Defender |
| U3 | 「應用程式與功能」的版本顯示 | (a) 接受不一致，文件、CLI、主控台同時顯示啟動器與代理程式版本；(b) 改寫 `DisplayVersion`；(c) 背景再跑 `msiexec` | **(a)**；不做 (b)、(c) |
| U4 | Authenticode 程式碼簽章 | (a) 不簽；(b) 簽章但不作為升級的信任條件；(c) 簽章並作為額外的必要檢查 | 信任條件維持 `allowed_signers`。依階段 4 的 Defender 與應用程式控制測試決定是否 (b)；若要支援 WDAC／AppLocker 管理的環境，(b) 很可能是「支援」的門檻。(b) 需調整可重現建置的比對與 `SHA256SUMS` 的產生順序，另有憑證費用與保管 |
| U5 | 預覽的隔離方式與預設值 | (a) 編譯期開關＋獨立的預覽通道，且 `upgrade.enabled` 預設 `false`；(b) 只靠本機設定 | **(a)** |
| U6 | 啟動器的形態 | (a) 同一個完整的 `pulse.exe` 以命令列分角色；(b) 獨立的小型啟動器（安裝為 `<根>\pulse.exe`），MSI 另帶種子 `seed\pulse.exe` | **(b)**（依審查改變原建議）。代價：安裝包多一份代理程式、wxs 多一個元件 |
| U7 | 槽的內容來源與解壓縮 | (a) 沿用 `windows-x86_64.zip`，新增 zip 解壓縮的依賴；(b) 系統的 `tar.exe`；(c) 另外發佈未壓縮的執行檔或 `.tar.gz` | **(a)**，前提是依賴通過 `cargo deny` 與授權審查，並實作第 4.5 節的解壓縮限制 |
| U8 | 信任錨的預設位置與交付 | (a) `<根>\allowed_signers`，以 `pulse upgrade trust --install` 或組態管理放入，不自動沿用舊位置；(b) 維持 `%ProgramData%` | **(a)**；`ALLOWEDSIGNERS=` 待階段 0 確認 wixl 的能力 |
| U9 | 支援的 Windows 版本 | — | 只列**實際在 VM 驗證過且仍在供應商支援期內**的版本。預覽預期為 Windows Server 2022 與 Windows 11；Windows Server 2025 需在 VM 補做才列入；2019 視客戶需求；2016 以前不列入 |
| U10 | 新增 `install.upgrade = "insecure_path"` | 新增／沿用 `unsupported` | **新增** |
| U11 | 階段 1（啟動器與槽）是否先單獨出貨 | 先出貨／不單獨出貨 | **不單獨出貨**（依審查撤回原建議）。最早在階段 4 的 VM 驗證後進預覽通道 |
| U12 | ADR-0003 的 Q1～Q6 與五點建議 | — | **已採納**（第 4.12 節）。剩下金鑰撤銷的回報欄位與是否限制後續提議，以 ADR-0003 修訂後的條文為準 |
| U13 | Windows 服務的失敗復原政策（含既有缺陷 X1～X4 的修正） | (a) 頻率有上限、持續重試（啟動器自行以 10／60／300 秒退避重啟子行程；結束代碼 2／3／4 不重啟；服務控制管理員只處理啟動器當掉）；(b) 次數有上限（`Restart ×3, None`），之後需人工 | **(a)**。X1～X4 的文件更正與 `ensure_failure_actions()` 的修正不待本 ADR 核定，建議併入最近的版本；現行 0.8～0.10 的服務在啟動器出貨前是否也改成 (a) 的等效設定，請一併決定 |
| U14 | E3（低權限代理程式）是否作為進入「支援」的門檻 | (a) 是；(b) 否，「支援」的文件明列第 4.6 節的不承諾 | **(a)**。做法在階段 0 實驗後另行設計與估計 |
| U15 | 試點的規模與門檻（第 7.3 節的數字） | 20 台／2 個發佈／各 4 週／成功率 ≥ 99%／`rollback_failed` 為 0 | 以這組為起點，依可取得的試點主機數調整 |
| U16 | 解除安裝後殘留的槽（M8） | (a) 文件要求手動刪除＋`pulse launcher reset --to-seed`；(b) 等 wixl 的能力確認後由 MSI 清除 | **(a)**，(b) 視階段 0 的結果再加 |

---

## 9. 參考資料

### 專案內

- [`crates/kairos-proto/REMOTE.md`](../../crates/kairos-proto/REMOTE.md)：第 2.1、2.4 節、第 4.2～4.6、4.8、4.10 節、第 5 節、第 12.4 節、第 17 節、第 18 節
- [`docs/adr/0003-rollback-bundle-trust.md`](0003-rollback-bundle-trust.md)（第 5.2、5.3、7 節）、[`docs/adr/0002-rollout-console-design.md`](0002-rollout-console-design.md)、[`docs/adr/reviews/0001-codex-review.md`](reviews/0001-codex-review.md)
- [`pulse/src/remote/install.rs`](../../pulse/src/remote/install.rs)、[`pulse/src/remote/upgrade/`](../../pulse/src/remote/upgrade/)
- [`pulse/src/winsvc.rs`](../../pulse/src/winsvc.rs)（`set_failure_actions`、`ensure_failure_actions`）、[`pulse/src/file_perms.rs`](../../pulse/src/file_perms.rs)、[`pulse/src/status_file.rs`](../../pulse/src/status_file.rs)、[`pulse/src/config_manager.rs`](../../pulse/src/config_manager.rs)（`default_allowed_signers_path`）
- [`crates/kairos-proto/src/release/mod.rs`](../../crates/kairos-proto/src/release/mod.rs)（`artifact_kind`）、[`kairos/src/remote/rollout.rs`](../../kairos/src/remote/rollout.rs)
- [`pulse/packaging/windows/pulse.wxs`](../../pulse/packaging/windows/pulse.wxs)、[`packaging/build-release-common.sh`](../../packaging/build-release-common.sh)（`PRODUCT_CODE`）、[`pulse/packaging/INSTALL.md`](../../pulse/packaging/INSTALL.md) A.3
- [`.github/workflows/ci.yml`](../../.github/workflows/ci.yml)、[`tests/systemd/`](../../tests/systemd/)、[`RELEASING.md`](../../RELEASING.md)、[`CHANGELOG.md`](../../CHANGELOG.md)

### 外部（2026-11-12 查閱）

- Microsoft Learn：[MoveFileExW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw)、[ReplaceFileW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-replacefilew)
- Microsoft Learn：[SERVICE_FAILURE_ACTIONSW](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/ns-winsvc-service_failure_actionsw)（超過動作數後重複最後一個動作）、[SERVICE_FAILURE_ACTIONS_FLAG](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/ns-winsvc-service_failure_actions_flag)
- Microsoft Learn：[File Versioning Rules](https://learn.microsoft.com/en-us/windows/win32/msi/file-versioning-rules)
- Microsoft Learn（審查引用，本次未重讀）：[Rollback Installation](https://learn.microsoft.com/en-us/windows/win32/msi/rollback-installation)、[Windows Shell Security Considerations](https://learn.microsoft.com/en-us/windows/win32/shell/sec-shell)
- GitHub Docs：[GitHub-hosted runners](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
- [Android A/B](https://source.android.com/docs/core/ota/ab)、[Chromium updater design doc](https://chromium.googlesource.com/chromium/src/+/main/docs/updater/design_doc.md)、[Squirrel install process](https://github.com/Squirrel/Squirrel.Windows/blob/develop/docs/using/install-process.md)、[Upgrade Elastic Agent](https://www.elastic.co/docs/reference/fleet/upgrade-elastic-agent)、[Fleet：Update agents](https://fleetdm.com/docs/using-fleet/update-agents)、[Tailscale `clientupdate_windows.go`](https://github.com/tailscale/tailscale/blob/main/clientupdate/clientupdate_windows.go)
- [CERT/CC：Finding Privilege Escalation Vulnerabilities in Windows using Process Monitor](https://vuls.cert.org/confluence/display/Wiki/2021/06/21/Finding+Privilege+Escalation+Vulnerabilities+in+Windows+using+Process+Monitor)、[CyberArk：Anti-Virus Vulnerabilities](https://www.cyberark.com/resources/threat-research/anti-virus-vulnerabilities-who-s-guarding-the-watch-tower)

### 未能查證、文中標為待驗證或假設的項目

1. 各 Windows 版本 `%ProgramFiles%`、`%ProgramData%`、磁碟根目錄的確切預設 ACE。
2. NTFS 上「寫入新檔 → 排清 → 改名為新檔名」在斷電後的實際持久性；VM 磁碟快取設定的影響。
3. wixl 0.106 對自訂動作、`RemoveFolderEx`、權限設定、`ServiceControl Start`、`MsiFileHash`、`MajorUpgrade` 排程的支援。
4. X1～X4 在實際 Windows 上的行為（X1 有文件依據但未實測；X3、X4 待驗證）。
5. M3、M4 中 Windows Installer 拒絕安裝的時點與結束代碼；MSI 交易斷電後的行為。
6. GitHub 代管的 Windows runner 上 Defender 即時保護是否開啟；以另建的非特權帳號在 runner 上執行攻擊腳本是否可行。
7. AppLocker／WDAC、Defender 對新路徑的未簽章執行檔（槽、種子、啟動器）的處理。
8. session 0 下通知子行程優雅結束的方式。
9. E3 的三種做法：從 LocalSystem 取得低權限權杖的方式、受限權杖的隔離強度、蒐集功能在低權限下的相容性。
10. Elastic Agent 與 Fleet Orbit 在 Windows 上的目錄與指標實作。
11. 評估版 Windows 的授權條款與有效天數。
12. ReFS、Server Core、非英文語系下的行為。
13. **假設**：多數 Windows 客戶以 MSI（SCCM／GPO）部署——儲存庫中沒有部署資料。
14. **假設**：停機「數秒」、槽的大小 `E` 與發佈檔大小 `Z` 的比例——都是估計，沒有量測。

---

## 10. 修訂紀錄

### 2026-11-12：依外部審查與 ADR-0003 修訂

**依據**：（a）codex 的審查 [`reviews/0001-codex-review.md`](reviews/0001-codex-review.md)（審查對象為 commit 4eb7086 的版本）；（b）[ADR-0003](0003-rollback-bundle-trust.md) 第 7 節對本 ADR Q1～Q6 的回答、第 7.2 節的條款與第 7.3 節的建議；（c）使用者的方向：採納 ADR-0003 的結論，Windows 以「回滾當下不重驗簽章、只檢查完整性；金鑰顯式撤銷要記錄高嚴重度狀態並影響後續升級」為準。

每一項都先回到程式或第一手文件確認。「確認」欄記錄確認的方式。

**重大問題（審查第 2 節）**

| 審查意見 | 確認 | 處置 | 修改的章節 |
|---|---|---|---|
| 2.1 SCM 不是「一天最多 3 次」；`ensure_failure_actions()` 只檢查有沒有動作 | 成立。Microsoft 文件：「If N is greater than cActions, the service controller repeats the last action」；`winsvc.rs` 設三個 `Restart`、重設期 86400 秒；`ensure_failure_actions()` 只看 `actions` 是否非空 | **採納**。更正事實並列為既有缺陷 X1～X3（另加讀 wxs 時發現的 X4）；啟動器改為自行以退避重啟子行程、結束代碼 2／3／4 不重啟、服務控制管理員只處理啟動器當掉。審查建議的 `Restart ×3, None` 列為 U13 的選項 (b)，未作為首選（理由見下方「部分採納」） | 1.2、4.4、5.3、6.1 S0／S1、7.1 階段 0、8 U13 |
| 2.2 指標損毀後「選最高版本」不安全；`confirmed` 與指標不是同一次寫入 | 成立（原 4.3 節的掃描規則與 4.6 節保留失敗槽 7 天互相衝突；ADR-0003 INV-1、INV-10） | **採納**。改為追加式狀態紀錄與單調遞增的 generation；取消掃描選槽與 `confirmed` 欄位；`slot.json` 封存後不再修改；定義寫入步驟與每個當機點的恢復結果；失敗的試用槽不保留執行檔 | 4.2、4.3、6.1 S4 |
| 2.3 MSI 種子直接成為 `active`，繞過健康確認 | 成立（原 4.4 節第 3 步、4.8 節） | **採納**。已有 `active` 時種子一律走試用、健康確認與自動切回；只有完全沒有有效紀錄時才建立初始 `active` | 4.8、4.3 |
| 2.4 LocalSystem 的代理程式可改寫一切，A/B 沒有權限隔離 | 成立（`winsvc.rs` `account_name: None`；wxs 未指定帳號；`file_perms.rs` 只解析允許項目） | **採納**。新增「安全承諾的範圍」：預覽只承諾良性當機、新版不健康與一般本機使用者；E3 列為進入「支援」的門檻並給出三種候選做法；啟動器不讀代理程式可寫的檔案來決定目標 | 1.3、3（E3、總表）、4.4、4.6、5.2、5.3、7.2、8 U14 |
| 2.5 MSI 與 OTA 同時發生沒有收斂規則；S7 沒有斷言；較舊的 MSI 是被 Windows Installer 擋下；`ProductCode` 不含預發佈標記 | 成立（wxs 的 `MajorUpgrade DowngradeErrorMessage`、`ServiceControl` 無 `Start`；`build-release-common.sh` 的 `MSI_VERSION=${VERSION%%-*}`） | **採納，一處調整**。定義 R1～R4 與 M1～M10 的預期結果，S7 逐項斷言。調整：沒有自訂動作的 MSI 取不到我們的鎖，所以收斂靠「啟動器啟動時偵測自己與種子的雜湊變更」而不是跨程序的全域鎖；鎖只用在啟動器、輔助程式與本機命令之間 | 1.2、4.8、4.9、6.1 S7 |
| 2.6 槽名稱不足以唯一識別內容 | 成立 | **採納**。槽 ID 加入發佈檔 SHA-256 的前 12 個字元；定義同名目錄、重試與清理的規則 | 4.2、6.1 S3 |
| 2.7 階段 1 先出貨違反實機原則 | 成立（啟動器是新的 LocalSystem 服務本體） | **採納**。撤回原建議；最早在 VM 驗證後進隔離的預覽通道 | 7.1、8 U11 |

**次要問題與不一致（審查第 3 節）**

| 審查意見 | 處置 | 修改的章節 |
|---|---|---|
| `REMOTE.md` 尚未列出 `slot`、`insecure_path`、`launcher`、`launcher_abi` | 採納：列為核定後必須同步的規格 | 4.11、7.4 |
| 信任錨位置改變是破壞性預設變更，需要遷移規則 | 採納（已確認 `config_manager.rs` 的預設與 wxs 沒有 `ALLOWEDSIGNERS`）：不自動沿用舊位置、`pulse upgrade trust --install`、影響面評估 | 4.5、5.2、8 U8 |
| 健康訊號與信任邊界改變，不是「只有路徑不同」 | 採納：新增對照表，原 4.7 節狀態機對應表中「只有路徑不同」的說法移除 | 4.7 |
| ADR-0002 並非獨立，統計需排除 `slot`、`tar` | 採納為交叉引用的要求（本次只改本檔，不修改 ADR-0002） | 表頭「相關」、7.4 |
| 磁碟空間公式未涵蓋峰值 | 採納：重算（平時 `3E`、峰值 `4E + 2Z`）並定清理順序 | 4.2 |
| `health_timeout_secs` 語意變更需明寫 | 採納：明寫為累計確認時間，加啟動次數上限與 7 天的牆上時間硬上限，並說明對 Kairos 的影響 | 4.7、4.11、5.2 |
| MSI 交易斷電後「下一次 `msiexec` 會接續回復」沒有依據 | 採納：改為未驗證 | 3 方案 A |
| 其餘（未版本化檔案規則、`MoveFileExW`、舊 Kairos 的相容性）審查確認正確 | 保留 | — |

**不正確或找不到依據的主張（審查第 5 節）**

| 審查意見 | 處置 | 修改的章節 |
|---|---|---|
| 「一天最多 3 次」不正確 | 採納（同 2.1） | 1.2、4.4 |
| 「ProductCode 依版本推導、不同版本不同」不精確 | 採納：改為依去掉預發佈標記的數字版本；衍生出 M4 與「測試用 MSI 必須用不同的數字版本」 | 1.2、4.8、6.1 |
| 「啟動器開機後一定會被 SCM 啟動」「不存在沒有可啟動版本的時間窗」過度保證 | 採納：全文改為附條件的陳述；比較總表該列改為「升級過程中是否會改寫正在使用的版本」 | 3、4.6、5.1 |
| 「指標不是舊完整狀態就是新完整狀態」設計未成立 | 採納：刪除該句，改以當機點表列出結果並註明前提 | 4.3 |
| 「多數 Windows 客戶使用 MSI」找不到依據 | 採納：標為假設 | 3 方案 B、9 |
| 「階段 1 沒有新的特權行為」不成立 | 採納（同 2.7） | 7.1 |
| 5.5～7 人週過於樂觀 | 採納：工程與自動測試 8～12 人週；E3 與試點另列 | 3 總表、7 |

**遺漏的方案、風險或驗證項目（審查第 6 節）**

| 審查意見 | 處置 | 修改的章節 |
|---|---|---|
| E3 列為正式的安全演進門檻 | 採納 | 4.6、7.2、8 U14 |
| 評估由 MSI 建立 `%ProgramData%\Pulse-Upgrade` 並套用 ACL | **部分採納**（見下） | 4.2 |
| 獨立的 `pulse-launcher.exe` | **採納，形式調整**（見下） | 4.1、8 U6 |
| 啟動器／槽格式的升級與降級矩陣 | 採納 | 4.9 |
| 磁碟滿、唯讀、寫入失敗等故障注入 | 採納 | 6.1 S4 |
| 封存完整性應涵蓋槽內所有檔案 | 採納：`slot.json` 列每個檔案的雜湊，V5 檢查全部且不允許多出檔案 | 4.2、4.5 |
| 解除安裝後保留槽再安裝的行為 | **部分採納**（見下） | 4.8 M8、8 U16 |
| 同版重試、重新簽署、兩個推送指向相同內容的測試 | 採納 | 6.1 S3 |
| ACL 測試擴充（繼承、拒絕、擁有者、掛接點、硬連結、8.3、大小寫、祖先被換掉、以控制代碼比對） | 採納 | 4.5、6.1 S5 |
| 「支援」前至少一種企業 EDR／應用程式控制 | 採納 | 6.3、7.3 |
| 階段 5 的規模與觀察期間 | 採納：給出建議值並列為 U15 | 7.3、8 U15 |
| 工作量分開列 | 採納 | 7 |

**對待決事項的意見（審查第 7 節）**

| 項目 | 處置 |
|---|---|
| U1、U2、U3、U4、U7、U8、U10 | 與審查一致；U3 補上「三處同時顯示兩個版本」，U7 補上解壓縮限制 |
| U5 | 採納：加上編譯期開關與發佈通道 |
| U6 | 採納：改為獨立的小型啟動器 |
| U9 | 採納：消除「2025 為基本」與階段 4 只要求 2022＋11 的不一致 |
| U11 | 採納：不單獨出貨 |
| U12 | 採納：ADR-0003 的 Q1～Q6、五點建議與 INV-10 |

**ADR-0003 第 7.3 節的建議**

| 建議 | 處置 | 修改的章節 |
|---|---|---|
| 1 掃描退路 | 採納其目的；做法依審查改為 generation 而不是 `confirmed_at`（牆上時間不單調），並直接取消掃描 | 4.3、4.12 |
| 2 明寫 Windows 沒有特權分離 | 採納 | 4.6 |
| 3 種子切換也走試用 | 採納 | 4.8 |
| 4 `release\` 對應 Linux 第二階段的 root 封存 | 採納，並補充 E3 之前的限制 | 4.12 |
| 5 失敗槽標記 `failed_at` | 採納其目的；做法改為不保留失敗槽的執行檔，比加標記更保守 | 4.2、4.3 |
| 回滾當下不重驗簽章；金鑰顯式撤銷記錄高嚴重度狀態並影響後續升級 | 採納；回報欄位以 ADR-0003 修訂後為準 | 4.5、4.12 |

**部分採納或不採納的項目與理由**

| 項目 | 處置 | 理由 |
|---|---|---|
| 2.1 的建議「設定 `Restart, Restart, Restart, None`」 | 部分採納：列為 U13 的選項 (b)，首選是 (a)「頻率有上限、持續重試」 | 事實更正全部採納。政策上，監控代理程式停在「已停止、等人處理」等於這台主機失去監控；(a) 以啟動器區分結束代碼（設定與憑證錯誤不重啟，與 Linux 一致），其餘以退避持續嘗試。這是政策選擇，交給使用者決定 |
| 2.5 的建議「跨程序全域鎖」 | 部分採納：鎖只用於啟動器、輔助程式與本機命令之間 | 沒有自訂動作的 MSI 無法參與我們的鎖；wixl 對自訂動作的支援未驗證。改以啟動時的雜湊偵測（R1）達到可斷言的收斂 |
| 由 MSI 建立 `%ProgramData%\Pulse-Upgrade` 並套用 ACL | 部分採納：記錄為已評估的替代方案，第一版不採用，E3 分離可寫資料時再評估 | 依賴 wixl 設定權限的能力（未驗證）；執行時才建立有被搶先建立的競態。`%ProgramFiles%` 的預設 ACL 不需要我們寫對任何東西 |
| 獨立的 `pulse-launcher.exe` | 採納實質，檔名不同：獨立的小型執行檔安裝為 `<根>\pulse.exe` | 審查的重點是「最難更新的元件不要帶整套代理程式的程式與相依」，這一點完全採納。沿用檔名是為了不改既有服務的 `ImagePath`、文件路徑與以檔案存在為準的偵測規則；完整的代理程式改放 `seed\` |
| 解除安裝後殘留的槽 | 部分採納：定義並測試行為、文件要求清除、提供 `pulse launcher reset --to-seed`；不保證「重裝即得到 MSI 的版本」 | 沒有自訂動作時啟動器無法區分 MajorUpgrade 與「移除後重裝」（兩者都換掉啟動器與種子）。wixl 的能力確認後可由 MSI 清除（U16） |
| ADR-0003 建議的 `confirmed_at`、`failed_at` 欄位 | 以更保守的做法取代 | 見上表 |

本次修訂沒有完全不採納的審查意見。
