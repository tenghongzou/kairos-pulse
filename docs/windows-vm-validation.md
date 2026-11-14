# Windows VM 驗證手冊（ADR-0001 第 6.2 節：第二層）

在使用者既有的 PVE 上以**評估版 Windows** 的 VM，驗證 GitHub Actions 的 runner 驗不到的項目：需要**重開機**的行為
（X3）、既有安裝的升級、Defender 即時保護、用戶端版 Windows。對應 [ADR-0001](adr/0001-windows-ota-upgrade.md)
第 1.2 節（X1～X4）、第 6.2 節與第 7.1 節的「階段 0」。

> **現況（2026-11-14）**：這份手冊是操作步驟，**沒有任何一步執行過**——還沒有建立 VM。`qm` 的指令是依 PVE 的
> 一般用法寫的參考，storage、bridge、VMID、ISO 的名稱都要換成實際的值。做完之後把結論回寫到 ADR-0001 第 9 節的
> 「實作現況」，並更新本檔最後的結果表。

## 1. 範圍

| 項目（ADR 6.2） | 階段 0 能做嗎 | 本手冊 |
|---|---|---|
| S0 全部項目在 Windows Server 2022 與 Windows 11 上重做 | 可以 | 第 4 節 |
| **X3**：「非 0 結束代碼也算失敗」旗標在重開機前後的行為 | 可以 | 第 5 節 |
| 正常重開機：服務自動啟動、失敗復原設定與狀態目錄的 ACL 保留 | 可以 | 第 6 節 |
| 既有安裝的遷移：0.8.0～0.10.x 的 MSI → 新版 MSI（X2、X4、F4 的升級路徑） | 可以（需要舊版的 MSI） | 第 7 節 |
| F4：由**真正的一般使用者帳號**預先建立 `%ProgramData%\Pulse` 時服務拒絕使用 | 可以 | 第 8 節 |
| Defender 即時保護與雲端保護開啟 | 可以（對象是現行的 `pulse.exe`；槽、種子、啟動器要等階段 1） | 第 9 節 |
| `pulse enroll` 之後服務自動重新啟動、實際連線與回報 | 需要 VM 連得到的 Kairos | 第 10 節 |
| 斷電（`qm stop`）在每個當機點、啟動器長時間執行、連續 20 次升級／切回 | **不行**：對象是啟動器與槽，階段 1 之後才有 | 第 11 節只列出，屆時補步驟 |

## 2. VM 規格

兩台，各自保留一個乾淨的快照：

| | Windows Server 2022（評估版） | Windows 11 Enterprise（評估版） |
|---|---|---|
| 用途 | ADR 第 7.1 節階段 4 要求的伺服器版 | 用戶端版（第 6.2 節） |
| vCPU／記憶體 | 2／4 GB | 2／4 GB（Windows 11 的最低需求；8 GB 較順） |
| 磁碟 | 64 GB，VirtIO SCSI | 64 GB，VirtIO SCSI |
| 韌體 | OVMF（UEFI） | OVMF（UEFI）＋ vTPM 2.0＋Secure Boot（Windows 11 的安裝需求） |
| 網路 | VirtIO；能連到放發佈檔的主機即可，**不需要對外** | 同左 |
| 其他 | QEMU guest agent（選用，方便 `qm guest exec`） | 同左 |

- 評估版的授權條款與有效天數**以 Microsoft 下載頁為準**（ADR 第 9 節第 11 項，尚未確認）。到期前完成，或以快照回復後重新計算的方式是否允許，請先看條款。
- **磁碟快取設定要記錄**（`qm config <vmid>` 的 `cache=`）：之後的斷電測試結果與它有關（ADR 第 6.2 節）。
- 語系：先用英文版（`sc.exe`、`icacls` 的輸出與文件一致）。繁體中文版是之後的加分項，請另開一台並在結果中標明。
- 需要 virtio-win 的驅動程式 ISO（安裝時載入 SCSI 與網路驅動程式）。

建立 VM 的參考指令（在 PVE 主機上；**未在這台 PVE 上執行過**，`<…>` 全部要換）：

```bash
qm create <vmid> --name kp-win2022 --ostype win11 --machine q35 --bios ovmf \
  --efidisk0 <storage>:1,efitype=4m,pre-enrolled-keys=1 \
  --tpmstate0 <storage>:1,version=v2.0 \
  --cpu x86-64-v2-AES --cores 2 --memory 4096 \
  --scsihw virtio-scsi-single --scsi0 <storage>:64,cache=none,discard=on \
  --net0 virtio,bridge=<bridge> --agent 1 \
  --ide2 <iso-storage>:iso/<windows 評估版>.iso,media=cdrom \
  --ide0 <iso-storage>:iso/virtio-win.iso,media=cdrom
qm start <vmid>            # 之後在主控台完成安裝
```

安裝完成後（每台做一次）：

```powershell
# 以系統管理員的 PowerShell
winver                                                   # 記下版本與組建
Get-ComputerInfo | Select-Object OsName, OsVersion, OsBuildNumber, CsSystemType | Format-List
Get-MpComputerStatus | Select-Object AMServiceEnabled, RealTimeProtectionEnabled, IsTamperProtected | Format-List
```

```bash
qm shutdown <vmid> && qm snapshot <vmid> clean --description "全新安裝、尚未安裝 Pulse"
```

之後每一節開始前都 `qm rollback <vmid> clean && qm start <vmid>`，除非該節另有說明。

## 3. 把檔案放進 VM

需要：repo 的 `tests/windows/Invoke-S0.ps1`、要驗證的 MSI 與 zip、兩個輔助 MSI（`tests/windows/make-test-msis.sh` 產生）、
第 7 節用的舊版 MSI。

- 發佈檔的來源：CI 的 artifact `release-<sha>` 與 `windows-test-msi-<sha>`，或在建置機上以 `sh packaging/release.sh` 產生後執行
  `sh tests/windows/make-test-msis.sh dist/pulse-*-windows-x86_64.zip dist/windows-test`。
- 傳進 VM：Windows 內建的 OpenSSH 用戶端（`scp <使用者>@<主機>:<路徑> C:\kp\`），或掛一個放了這些檔案的 ISO。放在 `C:\kp\`。
- **先驗證再安裝**：依 `pulse/packaging/INSTALL.md` 第 2 節核對 `SHA256SUMS`（至少 `Get-FileHash -Algorithm SHA256`），
  確定測到的是預期的建置。把雜湊記進結果。

## 4. S0（與 CI 相同的腳本）

```powershell
Set-ExecutionPolicy -Scope Process Bypass
cd C:\kp
.\Invoke-S0.ps1 -Msi .\pulse-<版本>-<commit>-windows-x86_64.msi -Zip .\pulse-<版本>-<commit>-windows-x86_64.zip `
    -LegacyMsi .\pulse-test-legacy-0.0.1.msi -NextMsi .\pulse-test-next-99.0.0.msi -OutDir C:\kp\s0-out
```

約 25 分鐘。收集整個 `C:\kp\s0-out\`（內容見 `tests/windows/README.md`）。`results.md` 中的 `FAIL` 逐項判讀：
是產品的問題，還是腳本的假設錯了（腳本在這之前沒有在 Windows 上跑過）。

## 5. X3：「非 0 結束代碼也算失敗」在重開機前後

背景：Microsoft 的文件說這個旗標的變更「takes effect the next time the system is started」。由 MSI 註冊的服務在
**第一次啟動時**才由 Pulse 自己設定旗標；`pulse service install` 則在註冊時設定。要回答的問題：

| # | 問題 |
|---|---|
| Q1 | MSI 安裝、**沒有重開機**：服務以非 0 結束代碼停止（設定錯誤，結束代碼 3）時，服務控制管理員會不會重新啟動它？ |
| Q2 | 同一台**重開機之後**：會不會？延遲是不是 10 秒？ |
| Q3 | `pulse service install`（zip）、沒有重開機：會不會？（S0 的 `x3-nonzero-exit-before-reboot.txt` 已記錄一次，這裡重做） |
| Q4 | 重開機之後，`sc.exe qfailure`、`sc.exe qfailureflag` 的內容有沒有變？ |

觀察用的指令（下面稱為「**觀察 N 秒**」；每 0.5 秒看一次服務狀態，有變化才印一行）：

```powershell
function Watch-Pulse([int]$Seconds, [string]$OutFile) {
    $t0 = Get-Date; $last = ''
    while (((Get-Date) - $t0).TotalSeconds -lt $Seconds) {
        $s = Get-CimInstance Win32_Service -Filter "Name='Pulse'"
        $now = "$($s.State) pid=$($s.ProcessId) exit=$($s.ExitCode) specific=$($s.ServiceSpecificExitCode)"
        if ($now -ne $last) { '{0,6:N0}s {1}' -f ((Get-Date) - $t0).TotalSeconds, $now | Tee-Object -FilePath $OutFile -Append; $last = $now }
        Start-Sleep -Milliseconds 500
    }
}
```

步驟（從 `clean` 快照開始）：

```powershell
mkdir C:\kp\x3 | Out-Null
msiexec /i C:\kp\pulse-<版本>-<commit>-windows-x86_64.msi /qn /l*v C:\kp\x3\install.log
Start-Sleep 20
sc.exe queryex Pulse     | Tee-Object C:\kp\x3\01-query-after-install.txt        # 預期 RUNNING（X4）
sc.exe qfailure Pulse    | Tee-Object C:\kp\x3\01-qfailure.txt                    # 預期 10000／60000／300000、重設期 86400
sc.exe qfailureflag Pulse | Tee-Object C:\kp\x3\01-qfailureflag.txt               # 預期 TRUE

# Q1：重開機前。以語法錯誤的設定讓服務以結束代碼 3 停止
sc.exe stop Pulse; Start-Sleep 5
Copy-Item "C:\Program Files\Pulse\pulse.toml" C:\kp\x3\pulse.toml.bak
Set-Content "C:\Program Files\Pulse\pulse.toml" '[server' -Encoding ASCII
sc.exe start Pulse
Watch-Pulse 120 C:\kp\x3\02-before-reboot.txt
sc.exe queryex Pulse | Tee-Object C:\kp\x3\02-query.txt
sc.exe stop Pulse; Copy-Item C:\kp\x3\pulse.toml.bak "C:\Program Files\Pulse\pulse.toml" -Force

# Q2、Q4：重開機
Restart-Computer -Force
```

重開機、登入後：

```powershell
sc.exe queryex Pulse      | Tee-Object C:\kp\x3\03-query-after-reboot.txt          # 預期 RUNNING（自動啟動）
sc.exe qfailure Pulse     | Tee-Object C:\kp\x3\03-qfailure.txt
sc.exe qfailureflag Pulse | Tee-Object C:\kp\x3\03-qfailureflag.txt
sc.exe stop Pulse; Start-Sleep 5
Set-Content "C:\Program Files\Pulse\pulse.toml" '[server' -Encoding ASCII
sc.exe start Pulse
Watch-Pulse 120 C:\kp\x3\04-after-reboot.txt       # 重開機後失敗計數從 0 起算：若會重啟，第一次約在 10 秒後、第二次再 60 秒後
sc.exe stop Pulse; Copy-Item C:\kp\x3\pulse.toml.bak "C:\Program Files\Pulse\pulse.toml" -Force
sc.exe start Pulse
Copy-Item "C:\Program Files\Pulse\logs" C:\kp\x3\logs -Recurse
Get-WinEvent -FilterHashtable @{LogName='System'; ProviderName='Service Control Manager'} -MaxEvents 200 |
    Where-Object Message -match 'Pulse' | Format-List TimeCreated, Id, Message | Out-File C:\kp\x3\scm-events.txt
```

Q3（另一次，從 `clean` 快照）：解開 zip 到 `C:\pulse`，`C:\pulse\pulse.exe service install --start`，之後與上面相同
（設定檔是 `C:\pulse\pulse.toml`；zip 沒有附 `pulse.toml`，直接建立即可，還原時刪除它）。

判讀：

| 觀察 | 結論 |
|---|---|
| `02-before-reboot.txt` 只有一次 `Running` → `Stopped`，之後不變 | Q1：重開機前**不會**重啟（X3 成立）。INSTALL 要寫明「MSI 安裝後、第一次重開機前，設定或憑證錯誤不會自動重試」 |
| `02-before-reboot.txt` 約 10 秒後出現新的 pid、再 60 秒後又一次 | Q1：重開機前就會重啟（X3 不成立，文件的敘述比實際保守） |
| `04-after-reboot.txt` 出現新的 pid | Q2：重開機後會重啟；記下實際的間隔 |
| `03-qfailure*.txt` 與 `01-*` 相同 | Q4：設定跨重開機保留 |

## 6. 正常重開機

接在第 5 節之後（或從 `clean` 安裝後重開機一次）：

```powershell
Get-CimInstance Win32_Service -Filter "Name='Pulse'" | Format-List State, ProcessId, StartMode, StartName, PathName
Get-Content "$env:ProgramData\Pulse\status.json" | ConvertFrom-Json | Select-Object pid, running, started_at, updated_at, working_dir
icacls "$env:ProgramData\Pulse"; (Get-Acl "$env:ProgramData\Pulse").Sddl
& "C:\Program Files\Pulse\pulse.exe" status; "exit=$LASTEXITCODE"
```

預期：服務 `Running`、`status.json` 的 `pid` 是重開機後的行程、`%ProgramData%\Pulse` 的擁有者與 ACL 與重開機前相同
（`O:BA`、只有 SYSTEM／Administrators 的 `(F)` 與 Users 的 `(RX)`）。

## 7. 既有安裝的升級（X2、X4、F4 的升級路徑）

需要實際發佈過的舊版 MSI（0.8.0、0.9.0、0.10.0 或 0.10.1）。**新版 MSI 的數字版本必須高於舊版**（ADR 第 4.8 節 M4）：
目前分支的版本號還是 0.10.1，與已發佈的 0.10.1 相同——要嘛用 0.10.0 以前的舊版，要嘛等版本號定下來之後的建置。

```powershell
mkdir C:\kp\mig | Out-Null
msiexec /i C:\kp\pulse-<舊版>-windows-x86_64.msi /qn /l*v C:\kp\mig\old-install.log
sc.exe start Pulse; Start-Sleep 30                         # 舊版的 MSI 安裝後不會啟動服務
sc.exe qfailure Pulse | Tee-Object C:\kp\mig\01-old-qfailure.txt          # 預期三個 10000（X1 的舊設定），或完全沒有（見下）
icacls "$env:ProgramData\Pulse" | Tee-Object C:\kp\mig\01-old-statedir.txt # 預期含 BUILTIN\Users 的寫入項目（繼承）
```

`01-old-qfailure.txt` 若沒有任何失敗動作，表示舊版由 MSI 註冊的服務從來沒有補上失敗復原（舊版校正時開啟服務的
控制代碼少了 `SERVICE_START` 權限，設定含「重新啟動」的失敗動作可能被拒絕）——這是讀程式得到的推測，請把實際看到的記下來。

在這裡關機做一個快照 `old-installed`（之後可以重複使用）。然後升級：

```powershell
msiexec /i C:\kp\pulse-<新版>-windows-x86_64.msi /qn /l*v C:\kp\mig\upgrade.log; "msiexec exit=$LASTEXITCODE"
Start-Sleep 30
sc.exe queryex Pulse  | Tee-Object C:\kp\mig\02-query-after-upgrade.txt   # X4：預期 RUNNING，不必手動啟動
sc.exe qfailure Pulse | Tee-Object C:\kp\mig\02-qfailure.txt              # X2：預期已校正為 10000／60000／300000
Select-String -Path "C:\Program Files\Pulse\logs\pulse-*" -Pattern '已校正服務|PLS-E8207' | Tee-Object C:\kp\mig\02-log.txt
& "C:\Program Files\Pulse\pulse.exe" status | Tee-Object C:\kp\mig\02-status.txt
```

預期：日誌有一則「已校正服務 Pulse 的失敗復原設定」；因為舊版建立的 `%ProgramData%\Pulse` 繼承了 Users 的寫入權限，
日誌與 `pulse status` 的「設定」列有 **`PLS-E8207`**，狀態檔寫在 `C:\Program Files\Pulse\logs\status.json`。
接著依 INSTALL.md A.3 處理並確認：

```powershell
sc.exe stop Pulse
cmd /c rd /s /q "%ProgramData%\Pulse"
sc.exe start Pulse; Start-Sleep 20
icacls "$env:ProgramData\Pulse" | Tee-Object C:\kp\mig\03-statedir-recreated.txt
& "C:\Program Files\Pulse\pulse.exe" status | Tee-Object C:\kp\mig\03-status.txt     # 不再有 PLS-E8207
```

另做一次修復：`msiexec /fa <新版 MSI> /qn` 之後 `sc.exe queryex Pulse`（預期 RUNNING）。

## 8. F4：真正的一般使用者預先建立狀態目錄（防禦面的確認）

S0 以「繼承預設 ACL 的目錄」代表不合規的狀態。這裡用真正的非特權帳號確認同一件事：**服務拒絕使用一般使用者擁有的目錄**。
只確認防禦有效，不做任何進一步的動作。從 `clean` 快照：

```powershell
# 系統管理員：建立一個不屬於 Administrators 的本機帳號
net user kp-std '<臨時密碼>' /add
# 以 kp-std 登入（或 runas /user:kp-std powershell），由它建立目錄：
mkdir C:\ProgramData\Pulse
# 回到系統管理員
mkdir C:\kp\f4 | Out-Null
(Get-Acl C:\ProgramData\Pulse).Owner | Tee-Object C:\kp\f4\01-owner.txt        # 預期是 kp-std
msiexec /i C:\kp\pulse-<版本>-<commit>-windows-x86_64.msi /qn; Start-Sleep 30
sc.exe queryex Pulse | Tee-Object C:\kp\f4\02-query.txt                         # 預期 RUNNING
Get-ChildItem C:\ProgramData\Pulse -Force | Tee-Object C:\kp\f4\02-statedir.txt # 預期是空的：服務沒有在裡面建立任何東西
Select-String -Path "C:\Program Files\Pulse\logs\pulse-*" -Pattern 'PLS-E8207' | Tee-Object C:\kp\f4\02-log.txt
Test-Path "C:\Program Files\Pulse\logs\status.json"                             # 預期 True
```

預期日誌的 `PLS-E8207` 訊息指出「擁有者是 S-1-5-21-…」。做完 `net user kp-std /delete`。另外兩個變化各做一次：
目錄換成 junction（`cmd /c mklink /J C:\ProgramData\Pulse C:\kp\elsewhere`，預期訊息為「是重新解析點」而且
`C:\kp\elsewhere` 維持空的）、目錄由系統管理員建立但以 `icacls C:\ProgramData\Pulse /grant "*S-1-5-11:(M)"` 放寬
（預期訊息為「ACL 讓 S-1-5-11 寫入」）。

## 9. Defender

```powershell
Get-MpComputerStatus | Format-List AMServiceEnabled, RealTimeProtectionEnabled, IoavProtectionEnabled, IsTamperProtected, AntivirusSignatureLastUpdated
Get-MpPreference | Format-List MAPSReporting, SubmitSamplesConsent        # 雲端保護；需要對外連線，沒有時記錄為未測
```

在即時保護開啟的狀態下重做第 4 節，然後：

```powershell
Get-MpThreatDetection | Format-List | Out-File C:\kp\defender-detections.txt
Get-MpThreat | Format-List | Out-File C:\kp\defender-threats.txt
Test-Path "C:\Program Files\Pulse\pulse.exe"
```

記錄：`pulse.exe`（未簽章）有沒有被偵測或隔離、MSI 安裝與服務啟動的時間有沒有明顯變長（`msi-install.log` 的起訖時間）。
這是 ADR 第 8 節 U4（Authenticode）的初步依據。

## 10. 有 Kairos 可連時（選做）

在 VM 連得到的主機上執行 Kairos（`kairos/packaging/INSTALL.md`），依 `pulse/packaging/INSTALL.md` A.3：

```powershell
Get-Content .\install-string.txt | & "C:\Program Files\Pulse\pulse.exe" enroll --install-string-file -
sc.exe queryex Pulse            # pulse enroll 成功後服務應已重新啟動（pid 與先前不同）
& "C:\Program Files\Pulse\pulse.exe" status
```

確認 0.8.0 起未驗證的項目：`pulse enroll` 之後服務自動重新啟動、連線與回報、主控台看到這台主機與它的系統資訊
（磁碟 I/O、行程、SMBIOS 在 LocalSystem 下的蒐集結果）。

## 11. 階段 1 之後才做的項目（先列出）

ADR 第 6.2 節的其餘項目以啟動器與槽為對象，現在沒有東西可測：每個當機點的正常重開機與斷電（`qm stop`，每點 20 次，
記錄磁碟快取設定）、啟動器連續執行 7 天、連續 20 次以上的升級／切回、槽與種子在 Defender 下的行為。屆時補進本手冊。

## 12. 要交回的東西與結果表

每台 VM 一個壓縮檔：`C:\kp\s0-out\`、`C:\kp\x3\`、`C:\kp\mig\`、`C:\kp\f4\`、Defender 的輸出，以及第 2 節記下的
Windows 版本、`qm config <vmid>`（含磁碟快取設定）、受測 MSI 的 SHA-256。

| 項目 | Windows Server 2022 | Windows 11 | 備註 |
|---|---|---|---|
| S0（`results.md` 的失敗數） | 未執行 | 未執行 | |
| X1：第 4 次以後仍重新啟動、間隔約 300 秒 | 未執行 | 未執行 | `kill-loop-msi.csv` |
| X2：升級後失敗動作被校正 | 未執行 | 未執行 | 第 7 節 |
| X3 Q1：MSI 安裝、重開機前非 0 結束代碼是否重啟 | 未執行 | 未執行 | 第 5 節 |
| X3 Q2：重開機後是否重啟 | 未執行 | 未執行 | |
| X3 Q3：`pulse service install`、重開機前 | 未執行 | 未執行 | |
| X4：安裝／修復／MajorUpgrade／舊版升級後服務在執行 | 未執行 | 未執行 | 第 4、7 節 |
| F4：新建目錄的 ACL、舊目錄被拒絕與處理、一般使用者預先建立被拒絕 | 未執行 | 未執行 | 第 4、7、8 節 |
| 重開機後服務自動啟動、設定與 ACL 保留 | 未執行 | 未執行 | 第 6 節 |
| Defender 即時保護開啟時沒有偵測或隔離 | 未執行 | 未執行 | 第 9 節 |
| `pulse enroll` 後自動重新啟動、連線與回報 | 未執行 | 未執行 | 第 10 節（選做） |
