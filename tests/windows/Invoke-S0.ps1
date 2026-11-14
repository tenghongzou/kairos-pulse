<#
.SYNOPSIS
  Pulse 的 Windows 基礎驗證（ADR-0001 第 6.1 節的 S0）：MSI 與 zip 的安裝、服務、失敗復原、升級、移除。

.DESCRIPTION
  以系統管理員執行。同一份腳本用在 GitHub Actions 的 Windows runner（.github/workflows/ci.yml 的
  windows-system job）與自管的 Windows VM（docs/windows-vm-validation.md）。說明與輸出格式見
  tests/windows/README.md。

  沒有 Kairos 可連：Pulse 以預設設定（127.0.0.1:8080）啟動並持續重試。因此不驗證 pulse enroll、連線、
  回報與集中設定；驗證的是安裝、服務的註冊與啟停、檔案位置、ACL、失敗復原與 MSI 的升級／修復／移除。

  這份腳本會安裝與移除 Pulse、刪除 C:\Program Files\Pulse 與 %ProgramData%\Pulse：只在測試機上執行。
  已安裝 Pulse 的主機會被拒絕（除非加 -Force）。

.PARAMETER Msi
  要驗證的 MSI（pulse-<版本>-<commit>-windows-x86_64.msi）。

.PARAMETER Zip
  內容相同的 zip；省略時略過 zip 的部分。

.PARAMETER LegacyMsi
  以 tests/windows/make-test-msis.sh 產生的「舊式」MSI（版本 0.0.1、沒有 ServiceControl Start）：
  用來重現 X4 並驗證從舊式 MSI 升級後服務會啟動。省略時略過。

.PARAMETER NextMsi
  以 make-test-msis.sh 產生的較新版本 MSI（版本 99.0.0）：驗證 MajorUpgrade。省略時略過。

.PARAMETER OutDir
  輸出目錄（results.json、results.md 與各項原始輸出）。

.PARAMETER KillCount
  連續強制結束服務行程的次數（X1）。預設 5：延遲依序 10、60、300、300、300 秒，約 17 分鐘。

.PARAMETER Skip
  要略過的階段：msi、legacy、zip、kill（只略過連續結束行程的部分）。

.PARAMETER Force
  主機上已有服務 Pulse 或安裝目錄時仍然執行（會被移除）。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Msi,
    [string]$Zip,
    [string]$LegacyMsi,
    [string]$NextMsi,
    [string]$OutDir = (Join-Path (Get-Location) 's0-out'),
    [int]$KillCount = 5,
    [string[]]$Skip = @(),
    [switch]$Force
)

Set-StrictMode -Version 2
$ErrorActionPreference = 'Continue'

$ServiceName = 'Pulse'
$InstallDir = Join-Path $env:ProgramFiles 'Pulse'
$StateDir = Join-Path $env:ProgramData 'Pulse'
$ZipDir = 'C:\pulse-s0-zip'
# 與 pulse/src/winsvc.rs 的 recovery 相同；改了那裡要一起改
$ExpectedDelaysMs = @(10000, 60000, 300000)
$ExpectedResetSecs = 86400
# 與 pulse/src/state_dir.rs 的規則相同：只有這些 SID 可以有寫入權限
$TrustedSids = @(
    'S-1-5-18', 'S-1-5-32-544',
    'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464',
    'S-1-3-0', 'S-1-3-1', 'S-1-3-4'
)
$OwnerSids = @('S-1-5-18', 'S-1-5-32-544', 'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464')
$WriteMask = 0x2 -bor 0x4 -bor 0x10 -bor 0x40 -bor 0x100 -bor 0x10000 -bor 0x40000 -bor 0x80000 -bor 0x10000000 -bor 0x40000000

$script:Results = New-Object System.Collections.ArrayList
$script:Phase = 'setup'

# ---- 結果記錄 ---------------------------------------------------------------

function Add-Result {
    param([string]$Outcome, [string]$Check, [string]$Detail = '')
    $row = [pscustomobject]@{
        phase   = $script:Phase
        outcome = $Outcome
        check   = $Check
        detail  = $Detail
        at      = (Get-Date).ToUniversalTime().ToString('o')
    }
    [void]$script:Results.Add($row)
    Write-Host ('[{0}] {1} {2}{3}' -f $Outcome, $script:Phase, $Check, $(if ($Detail) { "：$Detail" } else { '' }))
}

# 斷言：不成立時整體結果為失敗
function Assert-That {
    param([string]$Check, [bool]$Condition, [string]$Detail = '')
    if ($Condition) { Add-Result 'PASS' $Check $Detail } else { Add-Result 'FAIL' $Check $Detail }
}

# 觀察：只記錄，不影響結果（還不知道正確答案的項目，例如 X3）
function Add-Note {
    param([string]$Check, [string]$Detail = '')
    Add-Result 'INFO' $Check $Detail
}

function Save-Text {
    param([string]$Name, $Content)
    $path = Join-Path $OutDir $Name
    $Content | Out-File -FilePath $path -Encoding utf8
    return $path
}

function Invoke-Phase {
    param([string]$Name, [scriptblock]$Body)
    $script:Phase = $Name
    if ($Skip -contains $Name) {
        Add-Result 'SKIP' '整個階段' '以 -Skip 略過'
        return
    }
    try {
        & $Body
    }
    catch {
        Add-Result 'FAIL' '階段中途發生例外' ($_ | Out-String).Trim()
    }
}

# ---- 工具 -------------------------------------------------------------------

function Get-Svc {
    Get-CimInstance -ClassName Win32_Service -Filter "Name='$ServiceName'" -ErrorAction SilentlyContinue
}

function Wait-Svc {
    param([string]$State, [int]$TimeoutSec = 30)
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        $svc = Get-Svc
        if ($svc -and $svc.State -eq $State) { return $true }
        if (-not $svc -and $State -eq 'Absent') { return $true }
        Start-Sleep -Milliseconds 500
    }
    return $false
}

function Get-SvcState {
    $svc = Get-Svc
    if ($svc) { return $svc.State } else { return 'Absent' }
}

function Invoke-Msiexec {
    param([string[]]$Arguments, [string]$LogName)
    $log = Join-Path $OutDir $LogName
    $all = $Arguments + @('/qn', '/norestart', '/l*v', "`"$log`"")
    $p = Start-Process -FilePath 'msiexec.exe' -ArgumentList $all -Wait -PassThru
    return $p.ExitCode
}

function Invoke-Tool {
    # 執行命令列程式，輸出（含 stderr）存檔；回傳結束代碼
    param([string]$File, [string[]]$Arguments, [string]$OutName)
    $out = & $File @Arguments 2>&1 | ForEach-Object { "$_" }
    $code = $LASTEXITCODE
    if ($OutName) { [void](Save-Text $OutName $out) }
    return $code
}

function Save-ServiceConfig {
    param([string]$Tag)
    [void](Invoke-Tool 'sc.exe' @('qc', $ServiceName) "sc-qc-$Tag.txt")
    [void](Invoke-Tool 'sc.exe' @('queryex', $ServiceName) "sc-queryex-$Tag.txt")
    [void](Invoke-Tool 'sc.exe' @('qfailure', $ServiceName) "sc-qfailure-$Tag.txt")
    [void](Invoke-Tool 'sc.exe' @('qfailureflag', $ServiceName) "sc-qfailureflag-$Tag.txt")
}

function Save-Icacls {
    param([string]$Tag, [string[]]$Paths)
    $lines = New-Object System.Collections.ArrayList
    foreach ($p in $Paths) {
        [void]$lines.Add("=== $p")
        if (Test-Path -LiteralPath $p) {
            foreach ($l in (& icacls.exe $p 2>&1)) { [void]$lines.Add("$l") }
            try {
                $acl = Get-Acl -LiteralPath $p
                [void]$lines.Add("owner: $($acl.Owner)")
                [void]$lines.Add("sddl: $($acl.Sddl)")
                $item = Get-Item -LiteralPath $p -Force
                [void]$lines.Add("attributes: $($item.Attributes)")
            }
            catch { [void]$lines.Add("Get-Acl 失敗：$_") }
        }
        else { [void]$lines.Add('（不存在）') }
    }
    [void](Save-Text "icacls-$Tag.txt" $lines)
}

# 服務的失敗復原設定（登錄中的 SERVICE_FAILURE_ACTIONS：dwResetPeriod、兩個指標位置、cActions、
# 一個指標位置，共 20 bytes，之後是 cActions 個 { Type, Delay }）
function Get-FailureConfig {
    $key = "HKLM:\SYSTEM\CurrentControlSet\Services\$ServiceName"
    $props = Get-ItemProperty -LiteralPath $key -ErrorAction SilentlyContinue
    $result = [pscustomobject]@{ reset = $null; actions = @(); flag = $null; raw = '' }
    if (-not $props) { return $result }
    if ($props.PSObject.Properties.Name -contains 'FailureActionsOnNonCrashFailures') {
        $result.flag = [int]$props.FailureActionsOnNonCrashFailures
    }
    if ($props.PSObject.Properties.Name -contains 'FailureActions') {
        $bytes = [byte[]]$props.FailureActions
        $result.raw = ($bytes | ForEach-Object { $_.ToString('x2') }) -join ' '
        if ($bytes.Length -ge 20) {
            $result.reset = [BitConverter]::ToUInt32($bytes, 0)
            $count = [BitConverter]::ToUInt32($bytes, 12)
            $actions = @()
            for ($i = 0; $i -lt $count -and (20 + 8 * $i + 8) -le $bytes.Length; $i++) {
                $actions += , @([BitConverter]::ToUInt32($bytes, 20 + 8 * $i), [BitConverter]::ToUInt32($bytes, 24 + 8 * $i))
            }
            $result.actions = $actions
        }
    }
    return $result
}

function Test-FailureConfig {
    param([string]$Tag)
    $cfg = Get-FailureConfig
    $text = ($cfg.actions | ForEach-Object { '{0}/{1}ms' -f $_[0], $_[1] }) -join ', '
    $detail = "reset=$($cfg.reset) actions=[$text] flag=$($cfg.flag) raw=$($cfg.raw)"
    # SC_ACTION_RESTART = 1
    $ok = ($cfg.actions.Count -eq $ExpectedDelaysMs.Count)
    for ($i = 0; $ok -and $i -lt $ExpectedDelaysMs.Count; $i++) {
        if ($cfg.actions[$i][0] -ne 1 -or $cfg.actions[$i][1] -ne $ExpectedDelaysMs[$i]) { $ok = $false }
    }
    Assert-That "X1／X2：失敗動作為重新啟動 10／60／300 秒（$Tag）" $ok $detail
    Assert-That "X2：重設期 $ExpectedResetSecs 秒（$Tag）" ($cfg.reset -eq $ExpectedResetSecs) $detail
    Assert-That "X2：「非 0 結束代碼也算失敗」已開啟（$Tag）" ($cfg.flag -eq 1) $detail
}

# F4：目錄的擁有者、DACL、重新解析點（與 pulse/src/state_dir.rs 相同的規則）
function Get-DirViolations {
    param([string]$Path)
    $problems = @()
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { $problems += '是重新解析點' }
    $acl = Get-Acl -LiteralPath $Path
    $owner = $acl.GetOwner([System.Security.Principal.SecurityIdentifier]).Value
    if ($OwnerSids -notcontains $owner) { $problems += "擁有者 $owner" }
    foreach ($rule in $acl.GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier])) {
        if ($rule.AccessControlType -ne 'Allow') { continue }
        if (([int]$rule.FileSystemRights -band $WriteMask) -eq 0) { continue }
        $sid = $rule.IdentityReference.Value
        if ($TrustedSids -notcontains $sid) { $problems += "$sid 可寫入（$($rule.FileSystemRights)）" }
    }
    return $problems
}

function Read-StatusJson {
    param([string]$Path)
    try { return (Get-Content -LiteralPath $Path -Raw -Encoding UTF8 | ConvertFrom-Json) }
    catch { return $null }
}

function Wait-File {
    param([string]$Path, [int]$TimeoutSec = 40)
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if (Test-Path -LiteralPath $Path) { return $true }
        Start-Sleep -Milliseconds 500
    }
    return $false
}

function Save-Listing {
    param([string]$Name, [string[]]$Paths)
    $lines = New-Object System.Collections.ArrayList
    foreach ($p in $Paths) {
        [void]$lines.Add("=== $p")
        if (Test-Path -LiteralPath $p) {
            Get-ChildItem -LiteralPath $p -Recurse -Force -ErrorAction SilentlyContinue |
                ForEach-Object { [void]$lines.Add(('{0}  {1}' -f $_.FullName, $(if ($_.PSIsContainer) { '<DIR>' } else { $_.Length }))) }
        }
        else { [void]$lines.Add('（不存在）') }
    }
    return (Save-Text $Name $lines)
}

function Copy-PulseLogs {
    param([string]$Tag, [string]$WorkDir)
    $src = Join-Path $WorkDir 'logs'
    if (Test-Path -LiteralPath $src) {
        $dst = Join-Path $OutDir "logs-$Tag"
        New-Item -ItemType Directory -Force -Path $dst | Out-Null
        Copy-Item -Path (Join-Path $src '*') -Destination $dst -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# 移除服務與所有殘留（階段之間回到乾淨狀態）。不跟隨重新解析點：rd /s /q 只移除連結本身
function Reset-Host {
    $svc = Get-Svc
    if ($svc) {
        & sc.exe stop $ServiceName | Out-Null
        [void](Wait-Svc 'Stopped' 30)
        & sc.exe delete $ServiceName | Out-Null
        [void](Wait-Svc 'Absent' 30)
    }
    foreach ($d in @($InstallDir, $StateDir, $ZipDir)) {
        if (Test-Path -LiteralPath $d) { & cmd.exe /c rd /s /q "`"$d`"" | Out-Null }
    }
}

function Get-InstalledProducts {
    $roots = @(
        'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
    )
    @(Get-ItemProperty -Path $roots -ErrorAction SilentlyContinue |
            Where-Object { $_.PSObject.Properties.Name -contains 'DisplayName' -and $_.DisplayName -eq 'Pulse' })
}

function Uninstall-AllPulseProducts {
    param([string]$Tag)
    $codes = @()
    foreach ($p in (Get-InstalledProducts)) {
        $code = Invoke-Msiexec @('/x', $p.PSChildName) "msi-uninstall-$Tag-$($p.PSChildName).log"
        $codes += "$($p.PSChildName)=$code"
    }
    return $codes
}

# 連續強制結束服務行程（X1）
function Invoke-KillLoop {
    param([string]$Tag)
    $rows = New-Object System.Collections.ArrayList
    for ($i = 1; $i -le $KillCount; $i++) {
        $svc = Get-Svc
        if (-not $svc -or $svc.State -ne 'Running' -or -not $svc.ProcessId) {
            Assert-That "X1：第 $i 次結束行程前服務在執行" $false "狀態 $(Get-SvcState)"
            break
        }
        $before = [int]$svc.ProcessId
        Start-Sleep -Seconds 5
        $expected = $ExpectedDelaysMs[[Math]::Min($i, $ExpectedDelaysMs.Count) - 1] / 1000
        $killedAt = Get-Date
        Stop-Process -Id $before -Force -ErrorAction SilentlyContinue
        $deadline = $killedAt.AddSeconds($expected + 120)
        $restarted = $false
        $after = 0
        while ((Get-Date) -lt $deadline) {
            Start-Sleep -Seconds 2
            $now = Get-Svc
            if ($now -and $now.State -eq 'Running' -and $now.ProcessId -and [int]$now.ProcessId -ne $before) {
                $restarted = $true
                $after = [int]$now.ProcessId
                break
            }
        }
        $elapsed = [int]((Get-Date) - $killedAt).TotalSeconds
        [void]$rows.Add([pscustomobject]@{
                kill = $i; killed_at = $killedAt.ToUniversalTime().ToString('o'); pid_before = $before
                restarted = $restarted; pid_after = $after; seconds_until_running = $elapsed; expected_delay_seconds = $expected
            })
        $label = if ($i -ge 4) { "X1：第 $i 次（超過動作數之後）結束行程，服務仍重新啟動" } else { "X1：第 $i 次結束行程後服務重新啟動" }
        Assert-That $label $restarted "約 $elapsed 秒後回到執行中（設定的延遲 $expected 秒；輪詢間隔 2 秒）"
        if (-not $restarted) { break }
    }
    $rows | Export-Csv -Path (Join-Path $OutDir "kill-loop-$Tag.csv") -NoTypeInformation -Encoding UTF8
    Get-WinEvent -FilterHashtable @{ LogName = 'System'; ProviderName = 'Service Control Manager'; StartTime = (Get-Date).AddHours(-2) } -ErrorAction SilentlyContinue |
        Where-Object { $_.Message -match $ServiceName } |
        Select-Object TimeCreated, Id, Message |
        Format-List | Out-String -Width 400 | Out-File -FilePath (Join-Path $OutDir "scm-events-$Tag.txt") -Encoding utf8
}

# ---- 開始 -------------------------------------------------------------------

foreach ($input_file in @($Msi, $Zip, $LegacyMsi, $NextMsi)) {
    if ($input_file -and -not (Test-Path -LiteralPath $input_file -PathType Leaf)) {
        Write-Error "找不到 $input_file"
        exit 2
    }
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path
$Msi = (Resolve-Path -LiteralPath $Msi).Path
if ($Zip) { $Zip = (Resolve-Path -LiteralPath $Zip).Path }
if ($LegacyMsi) { $LegacyMsi = (Resolve-Path -LiteralPath $LegacyMsi).Path }
if ($NextMsi) { $NextMsi = (Resolve-Path -LiteralPath $NextMsi).Path }

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin = (New-Object Security.Principal.WindowsPrincipal($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Error '必須以系統管理員執行（已提升權限的 PowerShell）'
    exit 2
}
if (((Get-Svc) -or (Test-Path -LiteralPath $InstallDir)) -and -not $Force) {
    Write-Error "這台主機已有服務 $ServiceName 或 $InstallDir。這份腳本會移除它們；確定要在這台主機執行時加 -Force"
    exit 2
}

Invoke-Phase 'environment' {
    $os = Get-CimInstance Win32_OperatingSystem
    $lines = @(
        "os: $($os.Caption) $($os.Version) build $($os.BuildNumber)",
        "powershell: $($PSVersionTable.PSVersion)",
        "user: $($identity.Name)",
        "msi: $Msi",
        "zip: $Zip",
        "legacy_msi: $LegacyMsi",
        "next_msi: $NextMsi",
        "msi_sha256: $((Get-FileHash -LiteralPath $Msi -Algorithm SHA256).Hash)"
    )
    [void](Save-Text 'environment.txt' $lines)
    Add-Note '作業系統' "$($os.Caption) $($os.Version)"
    # ADR-0001 第 9 節「未能查證」第 6 項：runner 上 Defender 即時保護是否開啟
    try {
        $mp = Get-MpComputerStatus -ErrorAction Stop
        [void](Save-Text 'defender.txt' ($mp | Format-List | Out-String -Width 300))
        Add-Note 'Defender 即時保護' "RealTimeProtectionEnabled=$($mp.RealTimeProtectionEnabled) AntivirusEnabled=$($mp.AntivirusEnabled)"
    }
    catch { Add-Note 'Defender 即時保護' "Get-MpComputerStatus 無法執行：$_" }
    # ADR-0001 第 9 節第 1 項：預設 ACL
    Save-Icacls 'defaults' @("$env:SystemDrive\", $env:ProgramFiles, $env:ProgramData)
    Reset-Host
}

# ---- MSI --------------------------------------------------------------------

Invoke-Phase 'msi' {
    $exe = Join-Path $InstallDir 'pulse.exe'

    $code = Invoke-Msiexec @('/i', "`"$Msi`"") 'msi-install.log'
    Assert-That 'msiexec /i /qn 結束代碼為 0' ($code -eq 0) "結束代碼 $code"
    Assert-That '安裝了 pulse.exe' (Test-Path -LiteralPath $exe) $exe
    Assert-That '安裝了初始的 pulse.toml' (Test-Path -LiteralPath (Join-Path $InstallDir 'pulse.toml'))

    $svc = Get-Svc
    Assert-That '服務 Pulse 已註冊' ($null -ne $svc)
    if ($svc) {
        Save-ServiceConfig 'msi-installed'
        # 路徑有沒有加引號只記錄（C:\Program Files 有空白，沒有引號是「未加引號的服務路徑」問題）
        $expectedPath = "`"$exe`" service run"
        Assert-That '服務的命令列是 <安裝目錄>\pulse.exe service run' (($svc.PathName -replace '"', '') -eq ($expectedPath -replace '"', '')) "實際：$($svc.PathName)"
        Assert-That '服務的執行檔路徑有加引號' ($svc.PathName.StartsWith("`"$exe`"")) "實際：$($svc.PathName)"
        Assert-That '服務帳號為 LocalSystem' ($svc.StartName -eq 'LocalSystem') "實際：$($svc.StartName)"
        Assert-That '服務為自動啟動' ($svc.StartMode -eq 'Auto') "實際：$($svc.StartMode)"
        Assert-That '服務為獨立行程' ($svc.ServiceType -eq 'Own Process') "實際：$($svc.ServiceType)"
    }

    # X4：安裝後服務已啟動（尚未 enroll：連預設位址並持續重試）
    Assert-That 'X4：MSI 安裝後服務在執行' (Wait-Svc 'Running' 60) "狀態 $(Get-SvcState)"

    $version = (& $exe --version 2>&1 | Out-String).Trim()
    Assert-That 'pulse --version 可執行' ($LASTEXITCODE -eq 0) $version

    # 工作目錄是安裝目錄，不是 System32
    $logDir = Join-Path $InstallDir 'logs'
    Assert-That '日誌寫在安裝目錄的 logs\' ((Wait-File $logDir 40) -and @(Get-ChildItem -LiteralPath $logDir -Filter 'pulse-*' -ErrorAction SilentlyContinue).Count -gt 0) $logDir
    Assert-That 'System32 沒有 pulse.toml 或 logs\pulse-*' (-not (Test-Path (Join-Path $env:SystemRoot 'System32\logs\pulse-*')) -and -not (Test-Path (Join-Path $env:SystemRoot 'System32\agent.id')))

    # 本機狀態檔與狀態目錄（F4）
    $statusPath = Join-Path $StateDir 'status.json'
    Assert-That '狀態檔寫在 %ProgramData%\Pulse\status.json' (Wait-File $statusPath 60) $statusPath
    $status = Read-StatusJson $statusPath
    if ($status) {
        $svcPid = (Get-Svc).ProcessId
        Assert-That '狀態檔的 pid 是服務行程' ([int]$status.pid -eq [int]$svcPid) "status.json pid=$($status.pid)，服務 pid=$svcPid"
        Assert-That '狀態檔的工作目錄是安裝目錄' ("$($status.working_dir)".TrimEnd('\') -eq $InstallDir) "working_dir=$($status.working_dir)"
        Add-Note '狀態檔的設定警告' (($status.config.warnings | ForEach-Object { $_.code }) -join ', ')
        Assert-That 'F4：服務沒有回報狀態目錄不安全（PLS-E8207）' (-not (@($status.config.warnings | ForEach-Object { $_.code }) -contains 'PLS-E8207'))
    }
    else { Assert-That '狀態檔可解析' $false $statusPath }
    Save-Icacls 'msi-installed' @($InstallDir, (Join-Path $InstallDir 'logs'), (Join-Path $InstallDir 'pulse.toml'), $StateDir, $statusPath, (Join-Path $StateDir 'remote'))
    if (Test-Path -LiteralPath $StateDir) {
        $problems = @(Get-DirViolations $StateDir)
        Assert-That 'F4：%ProgramData%\Pulse 的擁有者與 DACL 合規（只有 SYSTEM／Administrators 可寫、不是重新解析點）' ($problems.Count -eq 0) ($problems -join '；')
    }
    $problems = @(Get-DirViolations $InstallDir)
    Add-Note '安裝目錄的擁有者與 DACL（ADR-0001 第 4.5 節的規則）' $(if ($problems.Count) { $problems -join '；' } else { '合規' })

    $code = Invoke-Tool $exe @('status', '--json') 'pulse-status-msi.json'
    Add-Note 'pulse status --json 的結束代碼（沒有 Kairos）' "$code"

    # 失敗復原（由 MSI 註冊的服務在第一次啟動時自行設定）
    Test-FailureConfig 'MSI 安裝、第一次啟動後'

    # 停止與重新啟動
    & sc.exe stop $ServiceName | Out-Null
    Assert-That '服務可以停止' (Wait-Svc 'Stopped' 30) "狀態 $(Get-SvcState)"
    [void](Invoke-Tool 'sc.exe' @('query', $ServiceName) 'sc-query-after-stop.txt')
    Start-Sleep -Seconds 20
    Assert-That '正常停止（結束代碼 0）不會觸發失敗復原' ((Get-SvcState) -eq 'Stopped') "20 秒後狀態 $(Get-SvcState)"
    $stopped = Read-StatusJson $statusPath
    if ($stopped) { Add-Note '停止後的狀態檔' "running=$($stopped.running) exit_code=$($stopped.exit_code)" }
    & sc.exe start $ServiceName | Out-Null
    Assert-That '服務可以重新啟動' (Wait-Svc 'Running' 30) "狀態 $(Get-SvcState)"

    # X1：連續強制結束服務行程
    if ($Skip -contains 'kill') { Add-Result 'SKIP' 'X1：連續結束服務行程' '以 -Skip kill 略過' }
    else { Invoke-KillLoop 'msi' }
    Save-ServiceConfig 'msi-after-kill-loop'

    # X4：修復
    if ((Get-SvcState) -ne 'Running') { & sc.exe start $ServiceName | Out-Null; [void](Wait-Svc 'Running' 30) }
    $code = Invoke-Msiexec @('/fa', "`"$Msi`"") 'msi-repair.log'
    Assert-That 'msiexec /fa 結束代碼為 0' ($code -eq 0) "結束代碼 $code"
    Assert-That 'X4：MSI 修復後服務在執行' (Wait-Svc 'Running' 60) "狀態 $(Get-SvcState)"

    # X4：MajorUpgrade
    if ($NextMsi) {
        Add-Content -LiteralPath (Join-Path $InstallDir 'pulse.toml') -Value '# s0-marker' -Encoding ASCII
        $idBefore = if (Test-Path (Join-Path $InstallDir 'agent.id')) { Get-Content -LiteralPath (Join-Path $InstallDir 'agent.id') -Raw } else { '' }
        $code = Invoke-Msiexec @('/i', "`"$NextMsi`"") 'msi-upgrade.log'
        Assert-That 'MajorUpgrade：msiexec /i 新版 結束代碼為 0' ($code -eq 0) "結束代碼 $code"
        Assert-That 'X4：MajorUpgrade 後服務在執行' (Wait-Svc 'Running' 60) "狀態 $(Get-SvcState)"
        $products = @(Get-InstalledProducts)
        Assert-That 'MajorUpgrade 後只剩一個 Pulse 產品' ($products.Count -eq 1) (($products | ForEach-Object { "$($_.PSChildName) $($_.DisplayVersion)" }) -join '；')
        Assert-That 'MajorUpgrade 保留修改過的 pulse.toml' ((Get-Content -LiteralPath (Join-Path $InstallDir 'pulse.toml') -Raw) -match 's0-marker')
        $idAfter = if (Test-Path (Join-Path $InstallDir 'agent.id')) { Get-Content -LiteralPath (Join-Path $InstallDir 'agent.id') -Raw } else { '' }
        Assert-That 'MajorUpgrade 保留 agent.id' ($idBefore -eq $idAfter) "升級前長度 $($idBefore.Length)、升級後 $($idAfter.Length)"
        Save-ServiceConfig 'msi-upgraded'
        Test-FailureConfig 'MajorUpgrade 後'

        # M3：安裝較舊的 MSI（ADR-0001 第 4.8 節；結束代碼與拒絕的時點待驗證，只記錄）
        $code = Invoke-Msiexec @('/i', "`"$Msi`"") 'msi-downgrade.log'
        Add-Note 'M3：在新版上安裝較舊的 MSI' "msiexec 結束代碼 $code；之後服務狀態 $(Get-SvcState)；產品 $((@(Get-InstalledProducts) | ForEach-Object { $_.DisplayVersion }) -join ',')"
    }
    else { Add-Result 'SKIP' 'X4：MajorUpgrade' '沒有 -NextMsi' }

    Copy-PulseLogs 'msi' $InstallDir

    # 移除
    $codes = @(Uninstall-AllPulseProducts 'msi')
    Assert-That 'msiexec /x 結束代碼為 0' (@($codes | Where-Object { $_ -notmatch '=0$' }).Count -eq 0 -and $codes.Count -ge 1) ($codes -join '；')
    Assert-That '移除後服務已刪除' (Wait-Svc 'Absent' 30) "狀態 $(Get-SvcState)"
    Assert-That '移除後 pulse.exe 已刪除' (-not (Test-Path -LiteralPath $exe))
    $listing = Save-Listing 'leftover-after-msi-uninstall.txt' @($InstallDir, $StateDir)
    $left = @(Get-ChildItem -LiteralPath $InstallDir -Force -ErrorAction SilentlyContinue | ForEach-Object { $_.Name })
    Add-Note 'msiexec /x 後安裝目錄留下的項目' ($left -join ', ')
    Assert-That '移除後保留 pulse.toml（Permanent）' ($left -contains 'pulse.toml') $listing
    Reset-Host
}

# ---- 舊式 MSI（沒有 ServiceControl Start）→ 現行 MSI --------------------------

Invoke-Phase 'legacy' {
    if (-not $LegacyMsi) { Add-Result 'SKIP' '整個階段' '沒有 -LegacyMsi'; return }
    $code = Invoke-Msiexec @('/i', "`"$LegacyMsi`"") 'legacy-install.log'
    Assert-That '舊式 MSI 安裝結束代碼為 0' ($code -eq 0) "結束代碼 $code"
    Start-Sleep -Seconds 10
    Add-Note 'X4（重現）：舊式 MSI 安裝後的服務狀態' (Get-SvcState)
    & sc.exe start $ServiceName | Out-Null
    Assert-That '舊式 MSI 的服務可以手動啟動' (Wait-Svc 'Running' 30) "狀態 $(Get-SvcState)"
    $code = Invoke-Msiexec @('/fa', "`"$LegacyMsi`"") 'legacy-repair.log'
    Start-Sleep -Seconds 10
    Add-Note 'X4（重現）：舊式 MSI 修復後的服務狀態' "msiexec 結束代碼 $code；狀態 $(Get-SvcState)"
    $code = Invoke-Msiexec @('/i', "`"$Msi`"") 'legacy-to-current.log'
    Assert-That '舊式 → 現行：msiexec /i 結束代碼為 0' ($code -eq 0) "結束代碼 $code"
    Assert-That 'X4：從沒有 Start 的 MSI 升級後服務在執行' (Wait-Svc 'Running' 60) "狀態 $(Get-SvcState)"
    Save-ServiceConfig 'legacy-upgraded'
    Copy-PulseLogs 'legacy' $InstallDir
    [void](Uninstall-AllPulseProducts 'legacy')
    Reset-Host
}

# ---- zip ＋ pulse service install -------------------------------------------

Invoke-Phase 'zip' {
    if (-not $Zip) { Add-Result 'SKIP' '整個階段' '沒有 -Zip'; return }
    $unpack = Join-Path $OutDir 'zip-unpack'
    if (Test-Path -LiteralPath $unpack) { Remove-Item -LiteralPath $unpack -Recurse -Force }
    Expand-Archive -LiteralPath $Zip -DestinationPath $unpack -Force
    $inner = @(Get-ChildItem -LiteralPath $unpack -Directory)
    $source = if ($inner.Count -eq 1) { $inner[0].FullName } else { $unpack }
    New-Item -ItemType Directory -Force -Path $ZipDir | Out-Null
    Copy-Item -Path (Join-Path $source '*') -Destination $ZipDir -Recurse -Force
    Remove-Item -LiteralPath $unpack -Recurse -Force
    $exe = Join-Path $ZipDir 'pulse.exe'
    Assert-That 'zip 內有 pulse.exe' (Test-Path -LiteralPath $exe) $exe

    $code = Invoke-Tool $exe @('service', 'install', '--start') 'zip-service-install.txt'
    Assert-That 'pulse service install --start 結束代碼為 0' ($code -eq 0) "結束代碼 $code"
    Assert-That 'zip：服務在執行' (Wait-Svc 'Running' 60) "狀態 $(Get-SvcState)"
    $svc = Get-Svc
    if ($svc) {
        Save-ServiceConfig 'zip-installed'
        Assert-That 'zip：服務的命令列含 service run --workdir <解開的目錄>' ($svc.PathName -like "*pulse.exe*service run --workdir*$ZipDir*") "實際：$($svc.PathName)"
        Assert-That 'zip：服務帳號為 LocalSystem' ($svc.StartName -eq 'LocalSystem') "實際：$($svc.StartName)"
        Assert-That 'zip：服務為自動啟動' ($svc.StartMode -eq 'Auto') "實際：$($svc.StartMode)"
    }
    Test-FailureConfig 'pulse service install 之後'
    Assert-That 'zip：日誌寫在解開的目錄的 logs\' ((Wait-File (Join-Path $ZipDir 'logs') 40))
    $statusPath = Join-Path $StateDir 'status.json'
    Assert-That 'zip：狀態檔寫在 %ProgramData%\Pulse\status.json' (Wait-File $statusPath 60) $statusPath
    Save-Icacls 'zip-installed' @($ZipDir, (Join-Path $ZipDir 'logs'), $StateDir, $statusPath)
    Add-Note 'zip 安裝目錄的擁有者與 DACL（ADR-0001 第 4.5 節的規則；C:\ 之下預期不合規）' ((@(Get-DirViolations $ZipDir)) -join '；')

    # 第二次 install：服務已存在，應以 PLS-E8105 失敗而不是改動既有服務
    $code = Invoke-Tool $exe @('service', 'install') 'zip-service-install-again.txt'
    Assert-That '服務已存在時 pulse service install 以結束代碼 1 拒絕' ($code -eq 1) "結束代碼 $code"

    # X3（重開機前的部分）：以非 0 結束代碼停止（設定檔語法錯誤，結束代碼 3）是否觸發重新啟動。只記錄
    & sc.exe stop $ServiceName | Out-Null
    [void](Wait-Svc 'Stopped' 30)
    $toml = Join-Path $ZipDir 'pulse.toml'
    $hadToml = Test-Path -LiteralPath $toml
    if ($hadToml) { Copy-Item -LiteralPath $toml -Destination "$toml.s0-bak" -Force }
    Set-Content -LiteralPath $toml -Value '[server' -Encoding ASCII
    $observed = New-Object System.Collections.ArrayList
    $startedAt = Get-Date
    & sc.exe start $ServiceName | Out-Null
    $pids = @{}
    $lastState = ''
    while (((Get-Date) - $startedAt).TotalSeconds -lt 100) {
        $now = Get-Svc
        $state = if ($now) { "$($now.State)/pid=$($now.ProcessId)/exit=$($now.ExitCode)/specific=$($now.ServiceSpecificExitCode)" } else { 'Absent' }
        if ($state -ne $lastState) {
            [void]$observed.Add(('{0,6:N1}s {1}' -f ((Get-Date) - $startedAt).TotalSeconds, $state))
            $lastState = $state
        }
        if ($now -and $now.ProcessId) { $pids[[int]$now.ProcessId] = $true }
        Start-Sleep -Milliseconds 500
    }
    [void](Save-Text 'x3-nonzero-exit-before-reboot.txt' $observed)
    [void](Invoke-Tool 'sc.exe' @('query', $ServiceName) 'sc-query-after-config-error.txt')
    Add-Note 'X3（重開機前）：設定錯誤（結束代碼 3）後 100 秒內的狀態變化' "看到 $($pids.Count) 個不同的行程；$(($observed | Select-Object -Last 4) -join ' | ')"
    & sc.exe stop $ServiceName | Out-Null
    [void](Wait-Svc 'Stopped' 30)
    if ($hadToml) { Move-Item -LiteralPath "$toml.s0-bak" -Destination $toml -Force } else { Remove-Item -LiteralPath $toml -Force }

    # F4 的防禦性回歸測試：%ProgramData%\Pulse 不合規（這裡以繼承 %ProgramData% 預設 ACL 的目錄代表：
    # 與 0.8.0～0.10.1 建立的目錄、一般使用者預先建立的目錄一樣，Users 可在其中建立檔案）時，
    # 服務必須拒絕使用它，而且不動其中既有的檔案
    if (Test-Path -LiteralPath $StateDir) { & cmd.exe /c rd /s /q "`"$StateDir`"" | Out-Null }
    New-Item -ItemType Directory -Path $StateDir | Out-Null
    $planted = Join-Path $StateDir 'status.json'
    $fake = '{"schema":"pulse.status-file/v1","updated_at":"2099-01-01T00:00:00.000Z","pid":1,"running":true,"version":"planted"}'
    Set-Content -LiteralPath $planted -Value $fake -Encoding ASCII
    $plantedHash = (Get-FileHash -LiteralPath $planted -Algorithm SHA256).Hash
    $loose = @(Get-DirViolations $StateDir)
    Add-Note 'F4：以預設繼承建立的 %ProgramData%\Pulse' ($loose -join '；')
    if ($loose.Count -eq 0) {
        Add-Result 'SKIP' 'F4：不合規的狀態目錄被拒絕' '這台主機的 %ProgramData% 預設 ACL 不讓一般使用者寫入子目錄，無法以這個方式測試'
    }
    else {
        & sc.exe start $ServiceName | Out-Null
        Assert-That 'F4：狀態目錄不合規時服務照常啟動' (Wait-Svc 'Running' 60) "狀態 $(Get-SvcState)"
        $fallback = Join-Path $ZipDir 'logs\status.json'
        Assert-That 'F4：狀態檔改寫在工作目錄的 logs\status.json' (Wait-File $fallback 60) $fallback
        $warned = $false
        $codes = ''
        for ($try = 0; $try -lt 15 -and -not $warned; $try++) {
            $real = Read-StatusJson $fallback
            if ($real) {
                $codes = (@($real.config.warnings | ForEach-Object { $_.code })) -join ', '
                $warned = $codes -match 'PLS-E8207'
            }
            if (-not $warned) { Start-Sleep -Seconds 1 }
        }
        Assert-That 'F4：狀態檔帶有 PLS-E8207 的設定警告' $warned "設定警告：$codes"
        Start-Sleep -Seconds 15
        Assert-That 'F4：服務沒有改寫不合規目錄中既有的 status.json' ((Get-FileHash -LiteralPath $planted -Algorithm SHA256).Hash -eq $plantedHash)
        $others = @(Get-ChildItem -LiteralPath $StateDir -Force | Where-Object { $_.Name -ne 'status.json' } | ForEach-Object { $_.Name })
        Assert-That 'F4：服務沒有在不合規的目錄中建立任何檔案或子目錄' ($others.Count -eq 0) ($others -join ', ')
        $log = (Get-ChildItem -LiteralPath (Join-Path $ZipDir 'logs') -Filter 'pulse-*' | Get-Content -Raw -Encoding UTF8) -join "`n"
        Assert-That 'F4：日誌有 PLS-E8207' ($log -match 'PLS-E8207')
        Push-Location $ZipDir
        $code = Invoke-Tool $exe @('status', '--json') 'pulse-status-f4.json'
        Pop-Location
        $shown = Get-Content -LiteralPath (Join-Path $OutDir 'pulse-status-f4.json') -Raw
        Assert-That 'F4：pulse status 沒有採用不合規目錄中的狀態檔' ($shown -notmatch 'planted') "結束代碼 $code"
        & sc.exe stop $ServiceName | Out-Null
        [void](Wait-Svc 'Stopped' 30)
    }
    # 依文件的處理方式修復：刪除目錄後啟動服務，服務以受保護的 ACL 重新建立
    & cmd.exe /c rd /s /q "`"$StateDir`"" | Out-Null
    & sc.exe start $ServiceName | Out-Null
    Assert-That 'F4：刪除不合規的目錄後服務可以啟動' (Wait-Svc 'Running' 60) "狀態 $(Get-SvcState)"
    Assert-That 'F4：服務重新建立 %ProgramData%\Pulse 並寫入狀態檔' (Wait-File $statusPath 60) $statusPath
    if (Test-Path -LiteralPath $StateDir) {
        $problems = @(Get-DirViolations $StateDir)
        Assert-That 'F4：重新建立的目錄合規' ($problems.Count -eq 0) ($problems -join '；')
    }
    Save-Icacls 'zip-state-dir-recreated' @($StateDir, $statusPath)

    Copy-PulseLogs 'zip' $ZipDir

    $code = Invoke-Tool $exe @('service', 'uninstall') 'zip-service-uninstall.txt'
    Assert-That 'pulse service uninstall 結束代碼為 0' ($code -eq 0) "結束代碼 $code"
    Assert-That 'zip：移除後服務已刪除' (Wait-Svc 'Absent' 30) "狀態 $(Get-SvcState)"
    [void](Save-Listing 'leftover-after-service-uninstall.txt' @($ZipDir, $StateDir))
    Reset-Host
}

# ---- 結果 -------------------------------------------------------------------

$script:Phase = 'summary'
$fail = @($script:Results | Where-Object { $_.outcome -eq 'FAIL' })
$pass = @($script:Results | Where-Object { $_.outcome -eq 'PASS' })
$info = @($script:Results | Where-Object { $_.outcome -eq 'INFO' })
$skip = @($script:Results | Where-Object { $_.outcome -eq 'SKIP' })
$script:Results | ConvertTo-Json -Depth 4 | Out-File -FilePath (Join-Path $OutDir 'results.json') -Encoding utf8
$md = New-Object System.Collections.ArrayList
[void]$md.Add("# Pulse Windows S0：通過 $($pass.Count)、失敗 $($fail.Count)、觀察 $($info.Count)、略過 $($skip.Count)")
[void]$md.Add('')
[void]$md.Add('| 階段 | 結果 | 項目 | 說明 |')
[void]$md.Add('|---|---|---|---|')
foreach ($r in $script:Results) {
    $detail = ("$($r.detail)" -replace '\r?\n', ' ' -replace '\|', '\|')
    [void]$md.Add("| $($r.phase) | $($r.outcome) | $($r.check -replace '\|', '\|') | $detail |")
}
$md | Out-File -FilePath (Join-Path $OutDir 'results.md') -Encoding utf8
if ($env:GITHUB_STEP_SUMMARY) { $md | Out-File -FilePath $env:GITHUB_STEP_SUMMARY -Append -Encoding utf8 }
Write-Host ''
Write-Host "通過 $($pass.Count)、失敗 $($fail.Count)、觀察 $($info.Count)、略過 $($skip.Count)；輸出在 $OutDir"
if ($fail.Count -gt 0) { exit 1 }
exit 0
