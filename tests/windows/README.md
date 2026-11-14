# Windows 系統層測試：S0 基礎驗證

ADR-0001（`docs/adr/0001-windows-ota-upgrade.md`）第 6.1 節的 **S0**：在真的 Windows 上安裝實際要發佈的 MSI 與 zip，
驗證 0.8.0 起「未在 Windows 實機驗證」的基礎項目、既有缺陷 X1～X4 的修正，以及安全稽核 F4（狀態目錄的 ACL）。

> **現況（2026-11-14）**：腳本已寫好並通過 PowerShell 的語法剖析，**尚未在任何 Windows 主機上執行過**。
> 第一次執行（CI 的 `windows-system` job 或 VM）的結果要當成「腳本本身也在被驗證」來讀：失敗可能是產品的問題，
> 也可能是腳本的假設錯了。確認過的項目請回寫到 ADR-0001 第 9 節的「實作現況」。

| 檔案 | 用途 |
|---|---|
| `Invoke-S0.ps1` | 測試本體（PowerShell 5.1 以上；檔案為 UTF-8 with BOM，Windows PowerShell 才讀得對中文） |
| `make-test-msis.sh` | 在 Linux（docker）上由發佈的 zip 產生兩個輔助 MSI：版本 0.0.1 的舊式 MSI（沒有 `ServiceControl Start`）與版本 99.0.0 的較新版本 |

同一份腳本用在兩個地方：

- **CI**：`.github/workflows/ci.yml` 的 `windows-system` job（`windows-2022`、`windows-2025`），使用 `package` job 的產出。
- **自管的 Windows VM**：`docs/windows-vm-validation.md`（另含需要重開機與斷電的項目）。

## 執行

以**系統管理員**的 PowerShell，在**測試機**上（腳本會安裝與移除 Pulse、刪除 `C:\Program Files\Pulse`、
`%ProgramData%\Pulse` 與 `C:\pulse-s0-zip`；已安裝 Pulse 的主機會被拒絕，除非加 `-Force`）：

```powershell
Set-ExecutionPolicy -Scope Process Bypass
.\tests\windows\Invoke-S0.ps1 `
    -Msi .\dist\pulse-<版本>-<commit>-windows-x86_64.msi `
    -Zip .\dist\pulse-<版本>-<commit>-windows-x86_64.zip `
    -LegacyMsi .\dist\windows-test\pulse-test-legacy-0.0.1.msi `
    -NextMsi .\dist\windows-test\pulse-test-next-99.0.0.msi `
    -OutDir .\s0-out
```

- `-Zip`、`-LegacyMsi`、`-NextMsi` 可省略（對應的階段略過）。`-Skip msi,legacy,zip,kill` 略過指定的階段
  （`kill` 只略過連續結束服務行程的部分）。`-KillCount`（預設 5）是連續結束服務行程的次數。
- 完整執行約 25 分鐘：連續結束服務行程 5 次要等 10＋60＋300×3 秒（失敗復原的延遲）。
- 結束代碼：0 沒有失敗的斷言、1 有失敗、2 不是系統管理員或參數有誤。

輔助 MSI（需要 docker 的 Linux 主機；CI 在 `package` job 產生）：

```bash
sh tests/windows/make-test-msis.sh dist/pulse-<版本>-<commit>-windows-x86_64.zip dist/windows-test
```

測試用的 MSI 必須使用**不同的數字版本**：`ProductCode` 由去掉預發佈標記的數字版本推導（ADR-0001 第 4.8 節 M4）。

## 階段與項目

結果分四種：`PASS`／`FAIL`（斷言）、`INFO`（只記錄的觀察：還不知道正確答案，或答案要由人判讀）、`SKIP`。

| 階段 | 內容 |
|---|---|
| `environment` | 作業系統版本、Defender 即時保護是否開啟（ADR 第 9 節第 6 項）、`C:\`／`%ProgramFiles%`／`%ProgramData%` 的預設 ACL（第 9 節第 1 項） |
| `msi` | `msiexec /i … /qn`；服務的命令列、帳號（LocalSystem）、啟動類型；**X4**：安裝後服務在執行；工作目錄與日誌在安裝目錄；狀態檔在 `%ProgramData%\Pulse`；**F4**：狀態目錄的擁有者與 DACL；**X1／X2**：登錄中的失敗動作為 10／60／300 秒、重設期 86400 秒、「非 0 結束代碼也算失敗」已開啟（由 MSI 註冊的服務在第一次啟動時自行設定）；停止（正常停止不觸發失敗復原）與重新啟動；**X1**：連續強制結束服務行程，記錄每一次是否重新啟動與經過的秒數（第 4 次以後是重點）；**X4**：`msiexec /fa` 修復後、MajorUpgrade 後服務在執行，升級保留修改過的 `pulse.toml` 與 `agent.id`；M3：安裝較舊的 MSI（只記錄）；`msiexec /x` 後服務與 `pulse.exe` 已移除，列出殘留的檔案 |
| `legacy` | 舊式 MSI（沒有 `Start`）安裝與修復後的服務狀態（**重現 X4**，只記錄）；升級到現行 MSI 後服務在執行 |
| `zip` | 解開 zip、`pulse service install --start`；命令列含 `--workdir`；失敗動作；再次 `install` 被拒；**X3（重開機前）**：以語法錯誤的 `pulse.toml` 讓服務以結束代碼 3 停止，記錄 100 秒內的狀態變化（只記錄）；**F4 的防禦性回歸測試**：`%ProgramData%\Pulse` 不合規（以繼承預設 ACL 的目錄代表）時，服務照常啟動、狀態檔改寫 `logs\status.json` 並帶 `PLS-E8207`、不改寫也不新增該目錄中的任何檔案、`pulse status` 不採用其中的狀態檔；刪除後服務以受保護的 ACL 重新建立；`pulse service uninstall` 與殘留 |

## 輸出（`-OutDir`；CI 上傳為 artifact `windows-s0-<os>-<sha>`）

| 檔案 | 內容 |
|---|---|
| `results.md`、`results.json` | 每個項目的階段、結果、說明（CI 另寫進 job summary） |
| `environment.txt`、`defender.txt` | 作業系統、PowerShell、MSI 的 SHA-256；`Get-MpComputerStatus` |
| `icacls-*.txt` | 各目錄的 `icacls` 輸出、擁有者、SDDL、屬性：`defaults`（系統預設）、`msi-installed`、`zip-installed`、`zip-state-dir-recreated` |
| `sc-qc-*.txt`、`sc-queryex-*.txt`、`sc-qfailure-*.txt`、`sc-qfailureflag-*.txt` | 服務設定與失敗復原的原始輸出 |
| `kill-loop-msi.csv`、`scm-events-msi.txt` | X1：每次結束行程的時間、是否重啟、經過秒數；服務控制管理員的事件 |
| `x3-nonzero-exit-before-reboot.txt` | X3：非 0 結束代碼後的狀態變化 |
| `msi-*.log`、`legacy-*.log` | `msiexec` 的詳細日誌 |
| `leftover-after-*.txt` | 移除後留在磁碟上的檔案 |
| `logs-msi\`、`logs-legacy\`、`logs-zip\` | Pulse 自己的日誌 |
| `pulse-status-*.json` | `pulse status --json` 的輸出 |

## 這裡驗不到的（沒有 Kairos、runner 不能重開機）

- `pulse enroll`、連線、回報、集中設定、`pulse enroll` 後的自動重新啟動：需要 Kairos（之後的 S2 以實際的 Kairos 驗證）。
- **X3 重開機後的部分**、X1／X2 的設定跨重開機是否保留、MSI 註冊的服務在重開機前後對非 0 結束代碼的反應：VM。
- 斷電、Defender 即時保護開啟時的行為、用戶端版 Windows、非英文語系：VM（`docs/windows-vm-validation.md`）。
- 以**非特權帳號**實際嘗試預先建立 `%ProgramData%\Pulse`（S5）：這裡只以「繼承預設 ACL 的目錄」代表不合規的狀態，
  斷言服務拒絕使用它；不包含任何提權或攻擊步驟。
- 0.8.0～0.10.1 **實際發佈的** MSI 到新版的升級：`legacy` 階段用的是由現行 `pulse.wxs` 拿掉 `Start` 產生的替代品，
  執行檔是新版。要驗證真正的舊版，把舊版的 MSI 以 `-Msi`、新版以 `-NextMsi` 傳入（數字版本需不同；舊版在 X1、X2、X4、F4 的斷言預期會失敗，要看的是升級之後的項目）。
