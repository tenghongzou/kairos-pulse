# 指標擴充（商業審計 F4）規格

本文件定義 Pulse 採集、Pulse ↔ Kairos 傳送、Kairos 儲存與呈現的**主機指標、行程排行與主機資產**，位於 monorepo 的 `crates/kairos-proto/METRICS.md`，與 [`PROTOCOL.md`](PROTOCOL.md)（8443 長連線）、[`ENROLL.md`](ENROLL.md)（8444 註冊通道）並列。兩邊共用的型別與純函式在 `kairos-proto`（`metrics`、`processes`、`inventory` 模組）；該 crate 的單元測試以 `include_str!` 讀入本文件，從「標準指標」節比對指標表、從「範例」節解析每一則訊息。改規格時改這裡與 `kairos-proto` 即可。

- 狀態：**衝刺 7 規格（0.8.0）**。共用型別、Pulse 端已完成；Kairos 端的協定、儲存（schema v8）、API 與告警已完成（實作與本文件的差異見 `kairos/STORAGE.md` 第 15 節與 `kairos/API.md` 第 5.9、5.10 節），主控台 S03／S04／S05／S08／S09 亦已完成（分工見第 16 節）。
- 線上格式的規則（`metrics` 陣列、features、錯誤碼）仍以 PROTOCOL.md 為準；本文件補上語意、來源、頻率、上限、儲存、API、告警與主控台。PROTOCOL.md 的標準指標表與本文件第 3 節由測試確認一致。
- 設計依據（設計包，唯讀）：`kairos/screens.md` S03 區塊 F、S04、S05（概覽「目前數值」、趨勢分頁、資產資訊分頁）、S09（指標門檻）、S19、§24 #5 #15；`design-system/copywriting.md` §2.1、§3、§5.4、§6；`design-system/README.md` §6.7、§6.16；`decisions/INTEGRATION.md` §25.1（保留期限與容量）；`kairos/alerts-ux.md` §2.2；`pulse/log-ux.md`（`sysinfo.snapshot`）；`HANDOFF.md` §4。本文件與設計包不同或設計包沒有的部分列在第 18 節。

## 1. 總覽

### 1.1 現況（0.7.0）

| 項目 | 現況 |
|---|---|
| 協定 | v2 的通用指標 `metrics: [{name, value, unit, labels}]`（A4）已完成；8 個標準指標 |
| Pulse | 以 `sysinfo` 0.39 採集，協商了 `metrics` 時送出 CPU、記憶體（總量／已使用）、每個掛載點的總容量／可用、每個網卡的累計收發位元組、溫度。只有 v1 的連線送 3 個欄位（CPU、記憶體總量、已使用，MB） |
| Kairos | SQLite 把每個指標（含 label）存成獨立序列（`series`），原始資料、5 分鐘與 1 小時彙總都有；網路計數器以原始累計值彙總，速率由讀取端計算 |
| 主控台 | 只畫 CPU 與記憶體；S05「資產資訊」分頁隱藏；告警的指標門檻只支援 `cpu`、`memory` |

商業審計說「只有 3 個指標」，指的是 v1 的 report 與主控台實際使用的部分。F4 要補的是：**採集的種類**（磁碟用量與 I/O、網路封包與錯誤、負載、置換空間、行程、uptime）、**資產**（OS、CPU、記憶體、廠牌型號序號、BIOS、磁碟、網卡 MAC／IP）、Kairos 的**速率計算、資產與行程的儲存、API、告警與主控台**。

### 1.2 本規格新增的東西

| 項目 | 線上格式 | 需要新 feature | 舊版的行為 |
|---|---|---|---|
| 19 個新的標準指標（第 3 節） | 沿用 `metrics` 陣列 | 否 | 0.2.0～0.7.0 的 Kairos 只檢查格式、原樣保存；主控台不顯示 |
| 行程排行（第 5 節） | report 的 `processes` 欄位 | `processes` | 沒有協商時不送 |
| 主機資產（第 6 節） | 新訊息 `inventory`，回 `ack` | `inventory` | 沒有協商時不送；舊版 Kairos 收到會回 `unknown_type` |

協定版本仍為 v1～v2，不提升（PROTOCOL.md「日後新增功能時加入新的 feature 名稱，不必提升版本號」）。

### 1.3 不在範圍內

- 自訂指標（應用程式送入的非 `system.*` 指標）：格式早已支援，Kairos 照存，但本衝刺不做專屬的主控台與告警。
- 每個 CPU 核心的使用率、每個行程的時序、容器（cgroup）層級的指標、GPU、SMART、服務狀態、軟體清單（已安裝套件）、修補程式。
- 資產的自訂欄位（保管人、財產編號、位置；S05 設計中有）：需要寫入權限與編輯介面，建議下一個衝刺（第 17 節 D6）。

## 2. 通則

### 2.1 命名、單位與 label

沿用 PROTOCOL.md「report（通用指標）」的規則：名稱為以 `.` 分隔的小寫識別字（`system.` 保留給標準指標），單位為 UCUM 風格（`%`、`By`、`s`、`ms`、`1`、`{packets}`），label 為字串對字串。

| label | 用於 | 值 |
|---|---|---|
| `mountpoint` | `system.filesystem.*` | Linux 掛載點（`/`、`/data`）；Windows 磁碟機根目錄（`C:\`） |
| `fstype` | `system.filesystem.*` | `ext4`、`xfs`、`NTFS`、`ReFS` 等（作業系統回報的原樣） |
| `device` | `system.disk.*` | Linux 核心的區塊裝置名稱（`nvme0n1p2`、`sdb1`、`dm-0`，不含 `/dev/`）；Windows 磁碟機代號（`C:`） |
| `device` | `system.network.*` | 介面名稱（Linux `eth0`、`ens18`；Windows 介面的別名，例 `Ethernet`） |
| `sensor` | `system.temperature` | 感測器名稱 |

同一個值在資產（第 6 節）的 `disks[].mountpoint`、`disks[].device`、`network_interfaces[].name` 中相同，主控台以此把時序與資產對應起來。

### 2.2 量測值與計數器

- **量測值**（gauge）：取樣當下的狀態，可直接畫圖、取平均、比門檻。
- **計數器**（counter）：自起點（開機、介面啟用、驅動程式載入）以來單調遞增的**累計值**，一律送原始累計值，**不在 Pulse 端換算速率**（取樣遺失或補送時，使用端仍能算出正確的平均速率）。
  - 使用端（Kairos）以相鄰兩筆相減求速率，規則由 `kairos_proto::counter_rate` 實作：時間沒有前進、值變小（**重置**：重新開機、介面重建、驅動程式重新載入、32 位元計數器溢位）、或 `system.uptime` 變小（中間重新開機過）時，**這一段沒有速率**（圖上為缺口，不補 0、不猜測）。
  - 計數器的值是 u64，JSON 以整數表示；Kairos 依 PROTOCOL.md 保留原本的數字表示，比較時不經過 f64。

### 2.3 取樣時間

每筆 report 的 `ts` 是取樣時間，report 內所有指標與 `processes` 都是同一次取樣的結果。CPU 使用率與磁碟、網路的速率都是「上一次取樣到這次取樣之間」的值，第一筆以 Pulse 啟動時的基準取樣為起點（PROTOCOL.md 與 Pulse README 的「基準取樣」）。

## 3. 標準指標

下表為全部 27 個標準指標（`kairos_proto::STANDARD_METRICS` 的順序；前 8 個是 0.2.0 起的指標，名稱、單位與範圍**凍結**）。「範圍」由伺服器檢查（不符時整筆 `invalid_report`）；label 只是慣例，伺服器不檢查。

| 名稱 | 單位 | 種類 | 範圍 | labels | 說明 |
|---|---|---|---|---|---|
| `system.cpu.usage` | `%` | 量測值 | 0～100 | 無 | 全機 CPU 平均使用率（上次取樣到這次取樣之間） |
| `system.memory.total` | `By` | 量測值 | ≥ 0 | 無 | 實體記憶體總量 |
| `system.memory.used` | `By` | 量測值 | ≥ 0 | 無 | 已使用記憶體，＝ total − available（不含可回收的快取） |
| `system.filesystem.total` | `By` | 量測值 | ≥ 0 | `mountpoint`、`fstype` | 檔案系統總容量 |
| `system.filesystem.available` | `By` | 量測值 | ≥ 0 | `mountpoint`、`fstype` | 一般使用者可用的空間（不含保留給 root 的部分） |
| `system.network.received` | `By` | 計數器 | ≥ 0 | `device` | 累計接收位元組 |
| `system.network.transmitted` | `By` | 計數器 | ≥ 0 | `device` | 累計傳送位元組 |
| `system.temperature` | `Cel` | 量測值 | 不限 | `sensor` | 元件溫度 |
| `system.cpu.load_1m` | `1` | 量測值 | ≥ 0 | 無 | 1 分鐘平均負載（只有 Linux） |
| `system.cpu.load_5m` | `1` | 量測值 | ≥ 0 | 無 | 5 分鐘平均負載（只有 Linux） |
| `system.cpu.load_15m` | `1` | 量測值 | ≥ 0 | 無 | 15 分鐘平均負載（只有 Linux） |
| `system.memory.available` | `By` | 量測值 | ≥ 0 | 無 | 可用記憶體（不必置換就能配置給新程式的量） |
| `system.swap.total` | `By` | 量測值 | ≥ 0 | 無 | 置換空間總量（Linux swap；Windows 分頁檔） |
| `system.swap.used` | `By` | 量測值 | ≥ 0 | 無 | 置換空間已使用量 |
| `system.filesystem.used` | `By` | 量測值 | ≥ 0 | `mountpoint`、`fstype` | 已使用量，＝ total − free（free 含保留給 root 的部分） |
| `system.filesystem.usage` | `%` | 量測值 | 0～100 | `mountpoint`、`fstype` | 使用率，＝ used ÷ (used + available)，與 `df` 的 Use% 相同（`kairos_proto::filesystem_usage`） |
| `system.disk.read` | `By` | 計數器 | ≥ 0 | `device` | 累計讀取位元組 |
| `system.disk.written` | `By` | 計數器 | ≥ 0 | `device` | 累計寫入位元組 |
| `system.disk.read_ops` | `{operations}` | 計數器 | ≥ 0 | `device` | 累計完成的讀取次數（只有 Linux） |
| `system.disk.write_ops` | `{operations}` | 計數器 | ≥ 0 | `device` | 累計完成的寫入次數（只有 Linux） |
| `system.disk.io_time` | `ms` | 計數器 | ≥ 0 | `device` | 累計忙碌時間（只有 Linux）；速率（ms/s）÷ 10 即忙碌百分比 |
| `system.network.packets_received` | `{packets}` | 計數器 | ≥ 0 | `device` | 累計接收封包數 |
| `system.network.packets_transmitted` | `{packets}` | 計數器 | ≥ 0 | `device` | 累計傳送封包數 |
| `system.network.errors_received` | `{errors}` | 計數器 | ≥ 0 | `device` | 累計接收錯誤數 |
| `system.network.errors_transmitted` | `{errors}` | 計數器 | ≥ 0 | `device` | 累計傳送錯誤數 |
| `system.process.count` | `{processes}` | 量測值 | ≥ 0 | 無 | 行程數，同 `ps -e` 的列數：含核心執行緒，不含一般執行緒（sysinfo 在 Linux 會把執行緒列為行程，Pulse 排除 `thread_kind()` 為 `Userland` 者） |
| `system.uptime` | `s` | 量測值 | ≥ 0 | 無 | 開機後經過的秒數（整數） |

### 3.1 與 v1 三欄位的對應

v1 連線（舊版 Kairos，或協商結果為 v1）只送 `cpu_usage`（%）、`total_memory`、`used_memory`（MB，= bytes ÷ 1048576 捨去），新指標都不送。Kairos 已把 v1 的三欄位對應到 `system.cpu.usage`、`system.memory.total`／`used`（× 1048576），本規格不變。

### 3.2 各平台的來源

Pulse 一律經由 `sysinfo`（第 4.1 節）；下表列出 `sysinfo` 0.39 實際讀取的來源，供實作與除錯參考。「—」表示該平台不送這個指標（不是送 0）。

| 指標 | Linux | Windows |
|---|---|---|
| `system.cpu.usage` | `/proc/stat` 兩次取樣差 | PDH 的處理器計數器（sysinfo） |
| `system.cpu.load_*` | `/proc/loadavg`（`System::load_average`） | —（sysinfo 在 Windows 以處理器佇列長度模擬，語意不同，不送） |
| `system.memory.*` | `/proc/meminfo` 的 `MemTotal`、`MemAvailable` | `GlobalMemoryStatusEx` 的 `ullTotalPhys`、`ullAvailPhys` |
| `system.swap.*` | `/proc/meminfo` 的 `SwapTotal`、`SwapFree` | `K32GetPerformanceInfo`：(CommitLimit − PhysicalTotal) × PageSize 與 (CommitTotal − PhysicalTotal) × PageSize（sysinfo 的定義，近似分頁檔） |
| `system.filesystem.*` | `/proc/mounts` ＋ `statvfs`（`f_blocks`、`f_bfree`、`f_bavail`） | `GetDiskFreeSpaceExW`（`TotalNumberOfBytes`、`TotalNumberOfFreeBytes`、`FreeBytesAvailable`） |
| `system.disk.read`／`written` | `/proc/diskstats`（磁區 × 512，sysinfo `Disk::usage`） | `IOCTL_DISK_PERFORMANCE`（sysinfo `Disk::usage`） |
| `system.disk.read_ops`／`write_ops`／`io_time` | `/proc/diskstats` 第 4、8、13 欄（Pulse 直接讀，sysinfo 沒有） | — |
| `system.network.*` | `/sys/class/net/<if>/statistics/*`（sysinfo `NetworkData`） | `GetIfTable2`（64 位元計數器） |
| `system.process.count` | `/proc` 下的行程目錄數（sysinfo 行程清單，排除一般執行緒） | `NtQuerySystemInformation(SystemProcessInformation)` |
| `system.uptime` | `/proc/uptime` | `GetTickCount64` |
| `system.temperature` | `/sys/class/hwmon` | WMI `MSAcpi_ThermalZoneTemperature`（多數主機沒有，需要系統管理員權限） |

## 4. 採集（Pulse）

### 4.1 函式庫：沿用 `sysinfo`，只在缺口直接讀 `/proc`

選定 **`sysinfo` 0.39**（已在依賴中，`default-features = false`，目前啟用 `system`、`disk`、`network`、`component`），理由：

1. **不新增依賴、不需要重新審核**：`cargo deny` 已通過（MIT；Windows 端依賴的 `windows` 0.62 為 MIT OR Apache-2.0）。本規格需要的 API（`Disk::usage`、`NetworkData` 的封包與錯誤計數、`mac_address`、`ip_networks`、`mtu`、`System::load_average`／`uptime`／`boot_time`／`physical_core_count`、`Cpu::brand`／`vendor_id`／`frequency`、行程的 `cpu_usage`／`memory`／`name`／`cmd`、`Product`／`Motherboard`（SMBIOS 的廠牌、型號、序號））都在已啟用的 feature 中，**不必新增 feature**。
2. **Windows 與 Linux 同一套程式**：Windows 的效能計數器、IP Helper、SMBIOS 由 sysinfo 處理，不必自己寫 FFI 或引入 `wmi`（COM，會帶入較多依賴）。
3. 直接讀 `/proc` 只用在 sysinfo 沒有的 3 個 Linux 指標（`/proc/diskstats` 的讀寫次數與忙碌時間），以及 BIOS 的版本與廠商（Linux `/sys/class/dmi/id/bios_*`，所有人可讀；Windows 登錄 `HKLM\HARDWARE\DESCRIPTION\System\BIOS` 的 `BIOSVendor`、`BIOSVersion`，沿用 Pulse 已有的 `RegGetValueW`）。

不採用的選項：全部直接讀 `/proc`（Windows 仍需另一套、重複造輪子）；`heim`（已停止維護，`cargo deny` 的 `unmaintained = "all"` 會擋）；WMI（`wmi` crate 依賴 COM 執行緒模型，虛擬服務帳號下的權限也待實測）。

不啟用 sysinfo 的 `user` feature（行程的使用者名稱）：它在 macOS 帶入 `objc2-open-directory`，且使用者名稱屬隱私資訊（第 8 節）。需要時 Linux 以 uid 查 `/etc/passwd`、Windows 以行程權杖查 SID 自行實作，列在第 17 節 D3。

### 4.2 頻率

| 項目 | 頻率 | 設定 |
|---|---|---|
| 標準指標 | 每次 report（`report.interval`，預設 5 秒） | 不變 |
| 行程排行 | 每 `processes.interval`（預設 60 秒，不小於 `report.interval`）附在當時的 report 上；其他 report 不帶 `processes` | 第 4.6 節 |
| 資產 | 連線建立（`hello_ack` 之後）送一次；之後每 `inventory.interval`（預設 1 小時）重新收集，摘要改變才送；距離上次送出滿 24 小時也送 | 第 6.3 節 |

磁碟、網路清單（掛載點、介面）每次取樣都重新整理（目前即如此），新掛載的磁碟與新介面下一筆就出現。行程的詳細資料（CPU 時間、記憶體、命令列）取樣成本較高，只在要送 `processes` 的那次取樣讀取；`system.process.count` 每次取樣都送，只列舉行程、不讀詳細資料（sysinfo `refresh_processes_specifics` 搭配 `ProcessRefreshKind::nothing()`）。行程的 CPU 使用率是兩次讀取詳細資料之間的平均，因此間隔就是 `processes.interval`。

### 4.3 篩選與每類上限

沿用目前的篩選：虛擬檔案系統（`tmpfs`、`overlay`、`squashfs` 等）、容量為 0 的掛載、重複掛載（同一裝置）、loopback 與虛擬介面（`veth*`、`docker*`、`br-*`、`virbr*`、`cni*`、`flannel*`、`cali*` 等）不送。介面名稱的篩選規則寫在 Pulse README，網路介面另可用 `collect.interface_exclude`（glob 清單）補充。

每筆 report 最多 256 個指標（PROTOCOL.md，**不可提高**：0.7.0 的 Kairos 超過就整筆拒絕）。各類上限與最壞情況：

| 類別 | 每項指標數 | 上限 | 最多指標數 |
|---|---|---|---|
| 無 label（CPU、負載 ×3、記憶體 ×3、置換空間 ×2、行程數、uptime） | — | — | 11 |
| 檔案系統（total、available、used、usage） | 4 | 16 個掛載點 | 64 |
| 磁碟 I/O（read、written；Linux 另有 read_ops、write_ops、io_time） | 5 | 8 個裝置 | 40 |
| 網路（received、transmitted、packets ×2、errors ×2） | 6 | 16 個介面 | 96 |
| 溫度 | 1 | 32 個感測器 | 32 |
| 合計 | | | **243** ≤ 256 |

目前 Pulse 的磁碟與介面上限為 32，F4 起分別改為 16（掛載點）、8（磁碟 I/O 裝置）、16（介面）。超過上限時依優先順序保留：掛載點為「`/`（Windows 為系統磁碟機）→ 容量大者優先」；磁碟 I/O 裝置依掛載點的順序；介面為「有 IPv4 位址者 → 累計流量大者優先」。被略過的項目在啟動時的 `sysinfo.snapshot` 日誌列出（`skipped`），之後每 24 小時最多記一次。

### 4.4 單行上限與截斷順序

單行上限 65536 bytes（PROTOCOL.md）。243 個指標在 label 值一般長度下約 30～40 KB；`processes` 不含命令列時約 1～2 KB、含命令列時最多約 22 KB。超過時依序捨棄，直到不超過：

1. `processes` 的 `command`（全部捨去）
2. `processes` 的清單縮成各 3 項，再捨去整個 `processes`
3. 溫度
4. 優先順序最低的介面、磁碟 I/O 裝置、掛載點（以「整組」捨去，例如一個掛載點的 4 個指標一起捨去，不留下不完整的組）

無 label 的 11 個指標一律保留。捨去的內容只影響這一筆，日誌以 `DEBUG` 記錄、每 10 分鐘最多一次 `WARN`（沿用 Pulse 目前「捨去標準指標以外的部分」的作法與訊息）。

### 4.5 權限與平台注意事項

| 平台 | 項目 | 說明 |
|---|---|---|
| Linux | 系統序號 `system.serial` | `/sys/class/dmi/id/product_serial` 為 root 才能讀（0400），而 Pulse 以 `pulse` 使用者、`CapabilityBoundingSet=` 執行，**讀不到**。建議 unit 加 `ExecStartPre=+`（以 root 執行、不受沙箱限制）把序號複製到 root 擁有的 `/run/pulse-hw/product_serial`（0.12.0 以前的 unit 複製到 `RuntimeDirectory=pulse` 之下），Pulse 優先讀該檔（第 17 節 D4）。讀不到時不送該欄位，啟動時記一次 `PLS-I8009` |
| Linux | 其他資產 | `sys_vendor`、`product_name`、`board_*`、`bios_*` 所有人可讀；`PrivateDevices=yes` 不影響 `/proc`、`/sys` 的讀取 |
| Linux | 行程 | unit 沒有 `ProtectProc=`，可以看到所有行程；命令列（`/proc/<pid>/cmdline`）所有人可讀。若管理者加上 `ProtectProc=invisible`，只看得到 pulse 自己的行程：此時 `system.process.count` 與 `processes` **不送**（看到的數目明顯少於 2 時判定為受限），記一次 `PLS-I8009` |
| Windows | 行程 | 虛擬服務帳號 `NT SERVICE\Pulse` 以 `PROCESS_QUERY_LIMITED_INFORMATION` 可取得名稱、CPU 時間與 Working Set；受保護的行程（例如 `csrss.exe`）的記憶體可能取不到（視為 0，不列入記憶體排行）。命令列需要 `PROCESS_VM_READ`，其他帳號的行程取不到 → 該行程不帶 `command` |
| Windows | 磁碟 I/O | `IOCTL_DISK_PERFORMANCE` 在虛擬服務帳號下能否開啟磁碟區**待實測**（設計包 `HANDOFF.md` §4 的「虛擬服務帳號讀效能計數器」）；取不到時不送 `system.disk.*`，記一次 `PLS-I8009` |
| Windows | 序號 | SMBIOS（`GetSystemFirmwareTable('RSMB')`）不需要系統管理員權限 |
| 兩者 | 取樣逾時 | 沿用 `PLS-W8005`（取樣逾時）：行程清單計入同一次取樣的逾時；逾時的那次不帶 `processes` |

### 4.6 設定鍵（`pulse.toml`，新增）

| 鍵 | 預設 | 範圍 | 說明 |
|---|---|---|---|
| `collect.filesystems` | `true` | | 送 `system.filesystem.*` |
| `collect.disk_io` | `true` | | 送 `system.disk.*` |
| `collect.network` | `true` | | 送 `system.network.*` |
| `collect.temperature` | `true` | | 送 `system.temperature` |
| `collect.max_filesystems` | 16 | 0～16 | 掛載點上限 |
| `collect.max_disks` | 8 | 0～8 | 磁碟 I/O 裝置上限 |
| `collect.max_interfaces` | 16 | 0～16 | 介面上限 |
| `collect.interface_exclude` | `[]` | 最多 32 個 glob | 額外排除的介面名稱 |
| `processes.enabled` | `true` | | 宣告 feature `processes` 並送 `system.process.count` |
| `processes.interval` | 60 | `report.interval`～3600 秒 | 行程排行的間隔 |
| `processes.top` | 5 | 1～10 | 依 CPU、依記憶體各取幾個 |
| `processes.command` | `"none"` | `none`／`executable`／`full` | 命令列：不送／只送執行檔完整路徑／送完整命令列（含參數，截斷到 1024 bytes），見第 8 節 |
| `processes.include_user` | `false` | | 送行程的使用者名稱（本衝刺不實作，見第 17 節 D3） |
| `inventory.enabled` | `true` | | 宣告 feature `inventory` |
| `inventory.interval` | 3600 | 300～86400 秒 | 重新收集資產的間隔 |
| `inventory.hardware_serials` | `true` | | 送 `system.serial`（第 8 節） |
| `inventory.network_addresses` | `true` | | 送介面的 MAC 與 IP 位址 |

`collect.*` 為 `false` 或上限為 0 時，該類指標不送；主控台顯示「這台主機未回報（Pulse 設定已關閉）」只能從「從未收到」推斷，不另外宣告。

## 5. 行程（report 的 `processes`）

雙方都宣告 `processes`、且協商了 `metrics` 時，report 可以另帶 `processes`（型別 `kairos_proto::ProcessList`）。沒有帶就是「這筆沒有行程資訊」，不代表沒有行程。

```json
"processes": {
  "top_cpu":    [{"pid": 2211, "name": "postgres", "cpu_usage": 9.41, "memory": 4294967296}],
  "top_memory": [{"pid": 1873, "name": "java", "cpu_usage": 3.2, "memory": 8589934592}]
}
```

| 欄位 | 型別 | 限制 |
|---|---|---|
| `top_cpu`、`top_memory` | 陣列 | 各 0～10 項（`MAX_TOP_PROCESSES`；Pulse 預設各 5），由高到低；同一清單內 `pid` 不重複，兩個清單可以有同一個行程。可省略（視為空陣列） |
| `pid` | u32 | |
| `name` | string | 執行檔名稱，不含路徑與參數（Linux `/proc/<pid>/comm`，最長 15 字元；Windows 映像名稱，例 `sqlservr.exe`）。1～256 bytes，字元限制同 `hostname`（Pulse 以 `sanitize_display_text` 處理） |
| `cpu_usage` | number | 占**整台主機** CPU 的百分比，0～100，與 `system.cpu.usage` 同一尺度：sysinfo 以單一核心為 100%，Pulse 以 `normalize_process_cpu` 除以邏輯處理器數。期間為上一次行程取樣到這次 |
| `memory` | u64 | 常駐記憶體 bytes（Linux RSS、Windows Working Set） |
| `user` | string | 選用，1～256 bytes。本衝刺 Pulse 不送 |
| `command` | string | 選用，1～1024 bytes，字元限制同 `hostname`。只在 `processes.command` 不是 `none` 時送；Pulse 以 `sanitize_process_command` 串接、替換禁止字元、截斷 |

不符限制時整筆 `invalid_report`（PROTOCOL.md 的規則：這筆不保存）。Pulse 送出前以同一個函式 `validate_process_list` 檢查，因此正常不會發生。`processes` 型別錯誤（不是物件、`pid` 為負數等）時 `bad_request`。沒有協商 `processes` 的連線上，這個欄位一律忽略（型別錯誤也不檢查）。

`processes` 是取樣的一部分：協商了 `durable` 時隨 report 存入 Pulse 的本機緩衝、斷線後補送（第 12 節）。

## 6. 資產（`inventory` 訊息）

### 6.1 訊息

雙方都宣告 `inventory` 時，Pulse 送出：

```json
{"type":"inventory","seq":2,"ts":"2026-10-20T02:00:00.120Z","inventory":{ ... }}
```

伺服器保存後回 `ack`（帶 `seq`）。`ts` 是收集資產的時間，限制同其他訊息的 `ts`。`inventory` 物件（`kairos_proto::Inventory`）的每個欄位都可省略：**省略代表這次沒有取得**（例如沒有權限讀序號），不代表該項目不存在，伺服器保留上一次的值（第 9.3 節）。物件中不認得的欄位忽略。

| 欄位 | 內容 |
|---|---|
| `os` | `family`（`linux`／`windows`）、`name`、`version`、`long_version`、`kernel`、`arch`；與 hello 的 `os` 相同的欄位取值方式相同（`System::name` 等），`long_version` 為 `System::long_os_version` |
| `boot_time` | 開機時間，RFC 3339 UTC（`System::boot_time`，精確到秒）。**不列入摘要**：重新開機不算資產變更 |
| `cpu` | `model`（`Cpu::brand`）、`vendor`（`Cpu::vendor_id`）、`physical_cores`、`logical_cores`、`frequency_mhz`（標稱時脈；取第一個核心） |
| `memory` | `total`、`swap_total`（bytes） |
| `system` | `vendor`、`product`、`serial`（SMBIOS type 1：sysinfo `Product`）、`board_vendor`、`board_product`（type 2：`Motherboard`）、`bios_vendor`、`bios_version`（第 4.1 節）。虛擬機的 `product` 通常是 `KVM`、`VMware Virtual Platform`、`Virtual Machine` 等，主控台據此不另外偵測虛擬化 |
| `disks` | 陣列，最多 64 項，每項 `mountpoint`（必填、不重複）、`device`、`fstype`、`total`、`kind`（`ssd`／`hdd`／`unknown`）、`removable`、`read_only`。篩選同 `system.filesystem.*`，但不受第 4.3 節的 16 個上限限制 |
| `network_interfaces` | 陣列，最多 64 項，每項 `name`（必填、不重複）、`mac`（小寫、`:` 分隔；全為 0 時不送）、`addresses`（CIDR 字串，例 `192.168.10.21/24`、`fe80::1/64`，最多 32 個，不含 IPv6 的 zone）、`mtu`。篩選同 `system.network.*` |

字串欄位一律 1～256 bytes、字元限制同 `hostname`（Pulse 以 `sanitize_display_text` 處理、取不到或處理後為空就不送）。限制的完整清單與檢查函式為 `kairos_proto::validate_inventory`。

### 6.2 伺服器的回應

| 情況 | 回應 |
|---|---|
| 保存成功（含內容與上次相同） | `ack` |
| 不是合法 JSON、欄位型別錯誤（例如 `inventory` 不是物件、`total` 不是 u64） | `bad_request`（`seq` 為 `null`，同其他訊息） |
| 內容不符 `validate_inventory` | `invalid_report`（帶 `seq`，不保存；Kairos 記 `KRS-W2108`） |
| 同一條連線兩則 `inventory` 間隔不到 60 秒（`INVENTORY_MIN_INTERVAL_MS`） | `storage_unavailable`（`inventory rate limit exceeded`，不保存） |
| 寫入失敗 | `storage_unavailable`（`report could not be stored`），同 report |
| 沒有協商 `inventory` | `unknown_type`（與舊版 Kairos 相同） |

`inventory` 計入每條連線的訊息速率限制與連續無效訊息上限，**不**計入 report 的速率限制。處理時間同 report（最多 8 秒內回應，PROTOCOL.md「逾時與離線判定」）。

### 6.3 Pulse 的送出規則

1. `hello_ack` 協商了 `inventory` 後，收集一次並送出（不論與上次是否相同：伺服器可能是新的、或資料被清除過）。
2. 之後每 `inventory.interval` 重新收集，以 `inventory_digest` 與「上次收到 `ack` 的摘要」比較，不同才送；距離上次收到 `ack` 滿 24 小時也送。
3. 收到 `invalid_report`：記 `PLS-W2108`，同一個摘要不再重送（等內容改變）。收到 `storage_unavailable`：5 分鐘後重送（不超過 `inventory.interval`）。斷線時在途的 `inventory` 視同沒有送出，重連後依第 1 點再送。
4. `inventory` **不經過本機緩衝**（它是狀態，不是時序；重連後一定會再送一次）。
5. 收集在 blocking 執行緒進行，逾時（10 秒）時這次不送、記 `PLS-W8005`；不影響 report 的取樣。
6. 單行上限：一般主機約 2～6 KB。超過 65536 bytes 時（例如上百個介面），依序捨去介面的 `addresses`（每個介面只留第一個 IPv4 與第一個 IPv6）、再捨去優先順序最低的介面與磁碟，直到不超過。

## 7. 相容性

### 7.1 feature 與版本

| feature | Kairos 宣告的條件 | Pulse 宣告的條件 |
|---|---|---|
| `metrics` | 不變（0.2.0 起） | 不變 |
| `processes` | 0.8.0 起，`storage.backend = "sqlite"`（JSONL 後端不宣告，第 9.5 節） | 0.8.0 起，`processes.enabled = true` |
| `inventory` | 0.8.0 起，`storage.backend = "sqlite"` | 0.8.0 起，`inventory.enabled = true` |

這兩個 feature **不在** `kairos_proto::SERVER_FEATURES` 中，由 Kairos 依儲存後端決定是否宣告（與 `renew` 的作法相同）；共用 crate 以 `Session::inventory`、`Session::processes` 表示協商結果。Kairos 0.8.0（SQLite）在 `hello_ack` 的 `features` 一律列出 `inventory`、`processes`（與 `metrics`、`durable` 相同，列出伺服器支援的；`renew` 等註冊 feature 才只在 client 也宣告時列出）。

### 7.2 相容矩陣

| Pulse | Kairos | 結果 |
|---|---|---|
| v1（0.1.0） | 0.8.0 | v1 三欄位，與今天相同。主控台：磁碟、網路等圖表與資產分頁顯示「這台主機的 Pulse 版本不回報此資訊（需要 0.8.0 以上）」；資產分頁以 hello 沒有主機資訊為由顯示空狀態 |
| 0.2.0～0.7.0 | 0.8.0 | v2＋`metrics`（＋`durable`）。已送的 8 個指標照常顯示：Kairos 以 `system.filesystem.total`／`available` **推算**使用率（第 9.2 節），網路以累計值算速率，因此磁碟使用率與網路流量的圖表、磁碟告警都可用；負載、置換空間、磁碟 I/O、行程、uptime、資產不可用（同上的提示）。資產分頁以 hello 的 `os` 顯示作業系統一列 |
| 0.8.0 | 0.2.0～0.7.0 | v2＋`metrics`：舊版 Kairos 不宣告 `processes`、`inventory`，Pulse 不送 `processes` 與 `inventory`；新的 19 個指標照送，舊版只檢查格式、**原樣保存**（升級 Kairos 後就能在趨勢中看到升級前的資料）。0.7.0 的主控台不顯示它們 |
| 0.8.0 | 0.1.0（v1） | v1 三欄位，與今天相同 |
| 0.8.0 | 0.8.0，`storage.backend = "jsonl"` | v2＋`metrics`（＋`durable`），不協商 `processes`、`inventory`；新指標寫入 JSONL `schema` 2 的行（格式不變） |
| 0.8.0，`processes.enabled = false` | 0.8.0 | 不協商 `processes`，也不送 `system.process.count` |
| 0.8.0 | 0.8.0 | 全部功能 |

舊端看到新內容時的行為（`kairos-proto` 的相容性測試以凍結的舊型別驗證）：

- 0.7.0 以前的 Kairos 收到 `{"type":"inventory",...}`：解析為不認得的 type（`serde(other)`），回 `unknown_type`。0.8.0 的 Pulse 只在協商後送出，因此不會發生；即使發生（例如設定錯誤）也只是非 fatal 的錯誤。
- 0.7.0 以前的 Kairos 收到帶 `processes` 的 report：多出的欄位被忽略。0.8.0 的 Pulse 只在協商後送出。
- 0.7.0 以前的 Kairos 以舊的檢查規則看待新指標：名稱、單位符合格式，不在舊的標準指標表中，因此只檢查格式、通過。數量不超過 256，單行不超過 65536（第 4.3、4.4 節），不會被整筆拒絕。

### 7.3 升級順序

任何順序都可以。建議**先升級 Kairos、再升級 Pulse**：Pulse 先升級時新指標仍會被舊版 Kairos 保存，但行程與資產要等 Kairos 升級後的下一次連線才開始（資產在 Kairos 升級、Pulse 重新連線後立即送出）。

## 8. 隱私

| 資訊 | 預設 | 理由與控制 |
|---|---|---|
| 行程名稱 | 送 | 只有執行檔名稱，維運必要（「哪個程式吃掉 CPU」） |
| 行程命令列 | **不送** | 參數常含密碼、token、連線字串、個資（例如 `mysql -p<密碼>`、`curl -H "Authorization: …"`）。`processes.command = "executable"` 只送執行檔路徑；`"full"` 才送參數，由管理者自行承擔。Kairos 不做遮罩（無法可靠辨識祕密），只在主控台以「Pulse 回報的命令列可能含敏感資訊」提示 |
| 行程使用者 | 不送 | 使用者名稱可能是員工帳號（個資）；本衝刺不實作 |
| MAC、IP 位址 | 送 | 資產管理與網路排障必要；`inventory.network_addresses = false` 可關閉 |
| 系統序號 | 送（讀得到時） | 資產管理（設計 S05、S19）必要；`inventory.hardware_serials = false` 可關閉 |
| 主機指紋 | 不變 | PROTOCOL.md「主機指紋」：只送雜湊。資產的序號與 MAC 是另外的、可關閉的欄位，不影響指紋的設計 |

- `pulse diag --redact strict`（Pulse CLI 規格）遮罩 IP 與主機名稱時，資產中的 IP、MAC、序號也一併遮罩。
- Kairos 的「刪除主機資料」（S6，0.9.0）一併刪除該主機的資產、資產變更歷史與行程快照（第 9 節的資料表都以 `host_id` 串接刪除；`kairos/PRIVACY.md` 第 4 節）。Kairos 另可依 `[privacy]` 遮蔽命令列中的秘密（預設）、捨棄或假名化行程的使用者、截斷資產的介面位址與 MAC（PRIVACY.md 第 2 節）。
- 稽核：查詢資產與行程不寫稽核紀錄（屬一般的 `hosts:read` 讀取），與趨勢查詢相同。

## 9. Kairos 端：儲存（schema v8 草案）

以下為 SQLite schema v8 的草案（`kairos/src/store/sqlite.rs` 新增 `SCHEMA_V8`，不修改既有的 migration）。實作線可以調整欄位，但需維持本節的語意並更新 `kairos/STORAGE.md`。

### 9.1 時序：沿用 `series`／`reports`／彙總表

新的標準指標不需要新的資料表：每個 (名稱, 單位, labels) 是一個序列，原始資料進 `reports`（format 2），彙總進 `rollup_5m`／`rollup_1h`，與目前相同。

**計數器的速率序列**（新增）：寫入計數器（`MetricKind::Counter`）時，Kairos 以 `counter_rate` 算出與同一主機、同一序列上一筆的速率，另存為**衍生序列**：

```sql
-- schema v8
ALTER TABLE series ADD COLUMN derived INTEGER NOT NULL DEFAULT 0;  -- 0 原始、1 由計數器算出的每秒速率
```

- 衍生序列的 `name`、`labels` 與原序列相同，`unit` 為 `<原單位>/s`（`By/s`、`{packets}/s`、`ms/s`），`derived = 1`。速率點的時間為後一筆的 `ts`。
- 原始累計值仍存在 `reports` 中（原始資料保留期限內可以重算），**但不再彙總**：v8 起計數器只彙總衍生的速率序列（累計值的平均、最小、最大沒有意義）。v7 以前已經寫入的累計值彙總保留到保留期限自然刪除。
- 每台主機的「上一筆」放在記憶體中（`host_id, series_id → CounterSample`）；Kairos 重新啟動後，該主機第一筆 report 以 `reports` 中時間最近且早於它的一筆作為上一筆（只查一次）。`system.uptime` 取自同一筆 report。
- 晚到的資料（`ts` 不晚於上一筆，例如 durable 補送與即時資料交錯）不產生速率點，也不更新「上一筆」；Pulse 依序送出，正常只在 Pulse 重新啟動、舊的 `boot_id` 還有積壓時發生。
- 每台主機的序列數上限 **2048**（新增，`storage.max_series_per_host`）：超過時新序列的指標不保存，其他照存並回 `ack`，記 `KRS-W4104`（每台主機每小時最多一次）。避免介面名稱不斷變化（容器網路）時序列無限增加；全域上限 1,000,000（`SERIES_FULL`）不變。

### 9.2 推算的使用率（相容 0.2.0～0.7.0 的 Pulse）

report 中某個掛載點有 `system.filesystem.total` 與 `available`、但沒有 `system.filesystem.usage` 時，Kairos 以 `(total − available) ÷ total × 100` 推算，存成同名的序列（`unit` 為 `%`，`derived = 2`：推算值）。推算值把保留給 root 的空間算成已使用，比 `df` 略高（ext4 預設 5%），主控台在數值旁標示「推算」。

### 9.3 資產

```sql
-- 每台主機目前的資產（最後一次收到的 inventory，與上一次合併後的結果）
CREATE TABLE host_inventory (
    host_id      INTEGER PRIMARY KEY REFERENCES hosts(host_id) ON DELETE CASCADE,
    digest       TEXT    NOT NULL,          -- inventory_digest(合併後的內容)
    body         TEXT    NOT NULL,          -- 合併後的 Inventory（JSON，不含 boot_time）
    collected_at INTEGER NOT NULL,          -- 最後一次 inventory 的 ts（Unix 毫秒；ts 不合理時用收到時間）
    received_at  INTEGER NOT NULL,          -- 最後一次收到的時間
    changed_at   INTEGER NOT NULL,          -- digest 最後一次改變的時間
    -- 主機清單（S04）篩選與排序用的冗餘欄位，與 body 一致
    os_family    TEXT, cpu_model TEXT, cpu_logical_cores INTEGER, memory_total INTEGER,
    vendor       TEXT, product TEXT, serial TEXT
);
-- 開機時間放在 hosts（每次 inventory 與 system.uptime 都可能更新，不算資產變更）
ALTER TABLE hosts ADD COLUMN boot_time INTEGER;   -- Unix 秒

-- 資產變更歷史（S05「資產變更歷史」）
CREATE TABLE inventory_changes (
    id          INTEGER PRIMARY KEY,
    host_id     INTEGER NOT NULL REFERENCES hosts(host_id) ON DELETE CASCADE,
    changed_at  INTEGER NOT NULL,           -- 收到變更的 inventory 的 collected_at
    changes     TEXT    NOT NULL            -- JSON：[{"path":"memory.total","before":8589934592,"after":17179869184}, ...]
);
CREATE INDEX inventory_changes_host ON inventory_changes(host_id, changed_at);
```

- **合併規則**：收到的 `inventory` 中**有出現**的頂層物件（`os`、`cpu`、`memory`、`system`）逐欄覆蓋，省略的欄位保留舊值；`disks`、`network_interfaces` 有出現時**整個取代**（陣列中消失的項目代表已移除）。
- **變更歷史**：合併後的摘要與 `host_inventory.digest` 不同時，算出逐欄的差異寫入 `inventory_changes`（第一次收到時不寫，只記「首次收到資產」）。路徑以 `.` 串接，陣列以鍵值標示：`disks[/data].total`、`network_interfaces[eth0].addresses`、`network_interfaces[eth1]`（整個項目新增或移除時 `before` 或 `after` 為 `null`）。
- 保留：變更歷史保留 400 天（同 `storage.rollup_1h_retention_days`），每台主機最多 1000 筆（超過時刪最舊的）。`host_inventory` 隨主機存在。
- hello 的 `os`（0.2.0 起的 Pulse）仍寫入 `hosts.os`（不變）。主控台的作業系統欄位優先用 `host_inventory`，沒有時用 `hosts.os`。

### 9.4 行程快照

```sql
CREATE TABLE process_snapshots (
    host_id  INTEGER NOT NULL REFERENCES hosts(host_id) ON DELETE CASCADE,
    ts       INTEGER NOT NULL,              -- report 的取樣時間（Unix 毫秒）
    body     BLOB    NOT NULL,              -- ProcessList 的 JSON（不壓縮；一般 0.5～2 KB）
    PRIMARY KEY (host_id, ts)
) WITHOUT ROWID;
```

- 每筆帶 `processes` 的 report 寫一列（與 report 同一個交易；durable 去重判定為重複的 report 不寫）。`top_*` 依規格的排序保存，不再處理。
- 保留期限同原始資料（`storage.raw_retention_days`，預設 7 天），容量上限的刪除順序中與原始資料同一層（先於 5 分鐘彙總）。
- 最新一筆另外放在記憶體（registry），供 S05 的即時顯示。

### 9.5 JSONL 後端

`storage.backend = "jsonl"` 沒有資產與行程的儲存：Kairos 不宣告 `processes`、`inventory`。新的標準指標照常寫入 `schema` 2 的行。JSONL 的鏡像（`storage.jsonl_mirror`）也不輸出 `processes` 與資產。

### 9.6 容量估算（更新 `STORAGE.md` 第 10 節與設計包 S02）

以預設 `report.interval` 5 秒、一般伺服器（2 個掛載點、2 個磁碟 I/O 裝置、2 個介面、無溫度）估算：

| 項目 | 0.7.0（17 個指標） | 0.8.0（41 個指標） |
|---|---|---|
| 每筆 report（含索引） | 約 240 bytes | 約 530 bytes |
| 原始資料每台每天 | 約 4.15 MB | 約 9.2 MB |
| 序列數（彙總） | 約 20 | 約 47（計數器只彙總速率） |
| 彙總每台每天 | 約 0.31 MB | 約 0.73 MB |
| 行程快照每台每天（每 60 秒、約 1 KB） | — | 約 1.4 MB |
| 資產與變更歷史 | — | 可忽略（每台數 KB） |
| **預設保留期限下每台合計** | 約 52 MB | 約 **129.1 MB**（原始 9.16 MB＋行程 1.44 MB 每天 × 8 天＝84.8 MB、5 分鐘彙總 21.7 MB、1 小時彙總 22.6 MB；設計包 §30.5 #1） |

`storage.max_total_bytes` 預設 64 GiB 時，約可完整保留 532 台（0.7.0 約 1,320 台）；超過時依既有規則先刪最舊的原始資料。這是第 17 節 D1 需要決定的事項。

## 10. Kairos 端：查詢 API

權限一律沿用 **`hosts:read`**（含群組範圍：範圍外的主機回 404，與既有主機端點相同），**不新增權限**（理由與替代方案見第 17 節 D2）。以下端點只接受 `GET`、不得帶本文，回應格式與錯誤沿用 `kairos/API.md` 的慣例；實作時寫入 API.md 第 5 節。

### 10.1 既有端點的變更

| 端點 | 變更 |
|---|---|
| `GET /api/v1/hosts/{id}/metrics/{name}` | 新參數 `transform`：`rate`（計數器的預設，回傳衍生的速率序列）、`none`（原始累計值，只有原始資料解析度，`resolution` 不是 `raw` 時 400 `invalid_parameter`）。量測值忽略此參數。回應的每個 `series` 另帶 `kind`（`gauge`／`counter`）、`derived`（`rate`／`estimated`／省略）。**行為變更**：計數器的預設從累計值改為速率（0.7.0 的主控台沒有使用計數器，API 為草稿） |
| `GET /api/v1/hosts/{id}/metrics` | 每個項目另帶 `kind`、`standard`（是否為標準指標） |
| `GET /api/v1/hosts`（S04） | 每台主機另帶 `inventory`：`{os_family, cpu_model, cpu_logical_cores, memory_total, vendor, product, serial}`（沒有資產時 `null`）、`boot_time`、`last_report.disk`：`{mountpoint, usage}`（使用率最高的掛載點）。新篩選 `os_family=`、新排序 `disk`、`uptime`。**`serial` 不出現在清單**，只在詳情（減少大量匯出時的外流面） |
| `GET /api/v1/hosts/{id}`（S05） | 另帶 `boot_time`、`uptime_secs`（最後一筆 report 的 `system.uptime`，或由 `boot_time` 算出）、`last_report` 增加 `filesystems`（每個掛載點的 total、used、available、usage）、`swap`、`load`、`process_count` |
| `GET /api/v1/overview`（S03 區塊 F） | `top_disk`（每台主機使用率最高的掛載點，前 10），與 `top_cpu`、`top_memory` 並列 |

### 10.2 新端點

| 端點 | 說明 |
|---|---|
| `GET /api/v1/hosts/{id}/inventory` | `{agent_id, supported, source, digest, collected_at, received_at, changed_at, boot_time, inventory}`。`supported`：這台主機最後一次連線是否協商了 `inventory`；`source`：`inventory`（Pulse 回報）或 `hello`（只有 hello 的 `os`，`inventory` 只含 `os`）或 `null`（都沒有，`inventory` 為 `null`） |
| `GET /api/v1/hosts/{id}/inventory/changes` | 參數 `limit`（1～200，預設 50）、`before`（游標，`id`）。回應 `{items: [{id, changed_at, changes: [{path, before, after}]}], next}`，新到舊 |
| `GET /api/v1/hosts/{id}/processes` | 參數 `at`（RFC 3339，預設現在）：回傳時間不晚於 `at` 的最近一筆快照 `{agent_id, sampled_at, top_cpu, top_memory, supported}`；超出保留期限或沒有資料時 `sampled_at` 為 `null`、清單為空。`command` 原樣回傳（第 8 節） |

### 10.3 CSV 匯出

S04 的 CSV 匯出（`KRS-E4301` 為失敗代碼）另加 OS、CPU 型號、核心數、記憶體、廠牌、型號、開機時間欄，**不含序號**（與清單 API 相同，減少大量匯出外流；序號只在主機詳情與 S19 合規報表，設計包 §30.4 #4、D47）；S05 趨勢的「匯出圖表資料 CSV」沿用目前的格式，欄名為指標名稱與 label（例 `system.filesystem.usage{mountpoint=/}`）。

## 11. Kairos 端：告警

### 11.1 指標門檻（`metric_threshold`）擴充

`params.metric` 目前只接受 `cpu`、`memory`（`kairos/src/alerts/rules.rs` 的錯誤訊息已預告 F4）。新增：

| `metric` | 值的來源 | 單位與門檻範圍 | 訊號的對象 |
|---|---|---|---|
| `disk` | `system.filesystem.usage`（或第 9.2 節的推算值） | %，0～100 | 每台主機的**每個掛載點**各自判斷、各自觸發與恢復；`params.mountpoints`（選用，glob 清單，例 `["/", "/data*", "C:\\"]`）限定範圍 |
| `swap` | `system.swap.used ÷ system.swap.total` | %，0～100 | 每台主機；`swap.total` 為 0 時沒有值（不觸發） |
| `load` | `system.cpu.load_5m ÷ inventory.cpu.logical_cores` | 每核心負載，0～100（倍） | 每台主機；Windows 與沒有資產的主機沒有值 |
| `network_rx`、`network_tx` | `system.network.received`／`transmitted` 的速率 × 8 | Mbps，0～1,000,000 | 每個介面；`params.devices`（選用，glob）限定範圍 |

- 其餘欄位（`op`、`threshold`、`for_secs`、`clear_threshold`、`clear_for_secs`、範圍 scope）與既有規則相同；`clear_threshold` 的方向檢查不變（違反時 `KRS-E5001`）。
- **評估方式不變**：每 5 秒以 registry 的 `LastReport` 評估。`LastReport` 擴充為保留最新一筆的使用率（每個掛載點）、置換空間、負載、介面速率（由第 9.1 節的速率計算同時更新）；離線主機沒有值。
- 預覽與回測（`POST /api/v1/alert-rules/preview`）使用 `rollup_5m` 中對應序列的平均值（磁碟為 `system.filesystem.usage`、網路為速率序列）。
- 主機沒有回報該指標（舊版 Pulse、Windows 的負載）時，範圍的「符合主機數」照算，但預覽另外列出「其中 N 台沒有回報這個指標」。
- 範本（`alerts-ux.md` §2.2「資源門檻預設不建立；提供範本」）新增：「磁碟使用率 > 90% 持續 10 分鐘，低於 85% 恢復」（警告）、「磁碟使用率 > 95% 持續 5 分鐘」（嚴重）。

### 11.2 主機重新開機（新的內建規則，建議）

`system.uptime` 變小或 `boot_time` 改變時發出事件「主機重新開機」（資訊等級，不需要確認、不通知，預設啟用；只在事件列表與 S05 的時間軸出現）。是否加入內建規則（目前 30 條）列在第 17 節 D5。

## 12. durable 補送

- **指標與行程**：都在同一筆取樣中，協商了 `durable` 時一起存入 Pulse 的本機緩衝、斷線後依 `report_seq` 順序補送，伺服器以 (agent_id, `boot_id`, `report_seq`) 去重（PROTOCOL.md）。不需要新的協定欄位。行程快照與 report 在同一個交易寫入，重複的 report 不會寫出重複的快照。
- **緩衝容量**：每筆紀錄從約 0.5～3 KB 增加到約 1.5～5 KB（帶 `processes` 的那筆再多約 1 KB）。`buffer.max_mb` 預設 64 MiB、`report.interval` 5 秒時約可保存 1～1.5 天（`buffer.max_records` 50,000 筆約 69 小時，先到者為準）。是否調整預設值列在第 17 節 D7。
- **補送的速率**：補送的 report 依計數器規則計算速率（第 9.1 節）。斷線期間的資料依序補送時速率正常；補送與即時資料交錯（Pulse 重新啟動）時，交錯的那一段沒有速率點。
- **資產**：不經過緩衝（第 6.3 節）。補送期間（`connection.replaying`）照常送 `inventory`。

## 13. 主控台

依設計包 S03、S04、S05、S09；設計包沒有的部分以〔待設計〕標示，列在第 18 節。數字與單位依 `copywriting.md` §6（容量用十進位 SI：1 GB = 10⁹ bytes；網路速率用 bit：Mbps；百分比小數 1 位），時間長度依 §5.4（最多兩個單位，例「12 天 4 小時」）。

### 13.1 S05 主機詳情

| 位置 | 內容 | 依據 |
|---|---|---|
| 概覽「目前數值」 | 既有的 CPU、記憶體之外：**磁碟**（使用率最高的掛載點，例「磁碟 C: 71%」，≥ 95% 標「將滿」；多個掛載點時旁邊加「＋2」展開）、**開機時間**（`uptime`，例「12 天 4 小時」）、置換空間、負載〔待設計〕 | S05 概覽、原型 `screens-monitor.js` |
| 概覽 Sparkline | 維持 CPU、記憶體 × 2（設計「最近 1 小時（Sparkline × 2）」） | S05 |
| 趨勢分頁 | 既有 CPU、記憶體之外：**磁碟使用率**（每個掛載點一條線，超過 3 條改小倍數）、**網路收發**（每個介面，Mbps，接收與傳送兩條）、磁碟讀寫（MB/s）〔待設計〕、負載〔待設計〕。缺口、離線區段、告警事件、平均＋最小／最大範圍、表格檢視、CSV 匯出沿用既有的 `trendChart` | S05 趨勢（「F4 之後加磁碟、網路」）、README §6.7 |
| 資產資訊分頁（取消隱藏） | 「硬體與作業系統」：作業系統（`long_version`）、核心／組建、CPU（型號＋核數）、記憶體、磁碟（每個「C: 512 GB（已用 x%）」）、網卡／MAC（＋IP）、序號、BIOS；「資產變更歷史」（例：記憶體從 8 GB 變 16 GB）。自訂欄位（保管人、財產編號、位置）本衝刺不做 | S05 資產資訊、原型 |
| 行程〔待設計〕 | 概覽或獨立分頁：依 CPU、依記憶體的前 N 個行程（名稱、PID、CPU %、記憶體），標示取樣時間；時間軸上點選某個時間可看當時的快照（`?at=`）。有命令列時以截斷＋展開顯示，並提示可能含敏感資訊 | 設計包沒有 |
| 空狀態 | 舊版 Pulse：「這台主機的 Pulse 版本不回報此資訊（需要 0.8.0 以上）」；新版但沒有資料：「尚未收到回報」（既有用語）；Pulse 關閉了該類採集時同「尚未收到回報」 | copywriting §7 |

### 13.2 S03、S04

- S03 區塊 F「資源使用前 10」的切換加上**磁碟**（設計已預留 `[切換：CPU｜記憶體｜磁碟(F4)]`），資料來自 `overview.top_disk`。
- S04 的 **OS** 欄（設計標 F4）改用資產的作業系統；可選欄位加入 CPU、記憶體總量、廠牌型號、開機時間〔可選欄位的清單待設計確認〕；篩選加作業系統家族。
- S19「資訊資產清冊」所需的「OS 與版本、硬體摘要」由 `GET /api/v1/hosts` 的 `inventory` 提供（S19 本身不在本衝刺）。

### 13.3 S09 告警規則

「指標門檻」的指標選單加上**磁碟使用率**（設計「F4 後加磁碟、網路」）、**網路流量**、置換空間、每核心負載〔後兩者待設計〕；磁碟與網路另有「掛載點／介面」的選擇（全部或指定）〔待設計〕。

## 14. 錯誤代碼

既有代碼沿用：`KRS-W2105`／`PLS-W2105`（report 不合法，含 `processes` 不合法）、`KRS-E4101`／`PLS-W4101`（無法寫入）、`PLS-W8005`（取樣逾時，含行程與資產的收集）、`KRS-E4003`（主控台無法載入資料，含資產與行程）、`KRS-E5001`（告警規則無法儲存，含新指標的門檻範圍錯誤）。

新增的代碼（設計包 `decisions/INTEGRATION.md` §30.3 正式分配，號碼與工程暫定的相同；連同打包線的 `KRS-W1306`、`KRS-E1307`、`PLS-E8105`、`PLS-E8106`，總數 213 → 221：Kairos 108、Pulse 113）：

| 代碼 | 標題 | 情境 | 類別 |
|---|---|---|---|
| `KRS-W2108` | 資產資訊不合法 | Kairos 以 `invalid_report` 拒絕 `inventory`（第 6.2 節） | 21 協定與版本（鏡像） |
| `PLS-W2108` | 資產資訊被伺服器拒絕 | Pulse 收到 `inventory` 的 `invalid_report`（第 6.3 節） | 21（鏡像） |
| `KRS-W4104` | 主機的指標序列數已達上限，部分指標未保存 | 第 9.1 節的每主機 2048 序列上限 | 41 寫入 |
| `PLS-I8009` | 部分系統資訊無法取得 | 權限或平台限制使序號、行程、磁碟 I/O 等取不到（第 4.5 節），啟動時記一次、列出取不到的項目 | 80 平台、systemd 與系統資訊（子類別改名） |

`8006`～`8008` 已保留給安裝前檢查（設計包 ERROR-CODES §4.1），因此取 `8009`。

## 15. 共用型別（`kairos-proto`）

| 項目 | 用途 | 使用端 |
|---|---|---|
| `STANDARD_METRICS`、`StandardMetric{kind, labels}`、`MetricKind`、`standard_metric`、`LEGACY_STANDARD_METRICS`、`METRIC_*` 常數 | 第 3 節的指標表 | 兩端 |
| `counter_rate`、`CounterSample` | 第 2.2 節的速率與重置規則 | Kairos（速率序列、告警） |
| `filesystem_usage` | 第 3 節的使用率公式 | Pulse（送出）、Kairos（第 9.2 節以外的計算） |
| `ProcessList`、`ProcessEntry`、`validate_process_list`、`normalize_process_cpu`、`sanitize_process_command`、`FEATURE_PROCESSES`、`MAX_TOP_PROCESSES` 等 | 第 5 節 | 兩端 |
| `ReportBody::processes`、`with_processes`、`process_list` | report 的 `processes` 欄位（與 `durable` 相同的分開解析） | 兩端 |
| `Inventory` 等、`InventoryReport`、`ClientMsg::Inventory`、`validate_inventory`、`inventory_digest`、`is_mac_address`、`is_ip_cidr`、`FEATURE_INVENTORY`、`INVENTORY_MIN_INTERVAL_MS` 等 | 第 6 節 | 兩端 |
| `Session::inventory`、`Session::processes` | 協商結果 | 兩端 |

## 16. 實作分工（衝刺 7 之後）

兩條線都以本文件與 `kairos-proto` 為準；需要改協定時先改本文件與共用 crate（另開規格分支），不在實作線各自改。

### 16.1 Pulse 線

工作：

1. 採集（`pulse/src/system_info.rs`）：第 3 節的新指標；`/proc/diskstats` 的 3 個 Linux 指標；第 4.3 節的上限與優先順序（取代目前的 32／32／32）；`collect.*` 設定。
2. 截斷（`pulse/src/protocol.rs`）：以第 4.4 節的「整組、依優先順序」取代目前的「從尾端減半」。
3. 行程：`processes.*` 設定、只在 `processes.interval` 的那次取樣更新行程清單、`normalize_process_cpu`、命令列的三種模式、`ProtectProc` 受限的判定；`Sample` 增加 `processes`（緩衝的紀錄格式向下相容：舊紀錄沒有這個欄位時視為沒有）。
4. 資產：收集（sysinfo＋BIOS 的 `/sys`／登錄）、第 6.3 節的送出規則、`inventory.*` 設定；序號的 `ExecStartPre` 檔案（若 D4 採用，改 `packaging/pulse.service` 與 INSTALL.md）。
5. hello 的 `features` 依設定加入 `inventory`、`processes`；`pulse status` 的 features 列出協商結果。
6. 代碼 `PLS-W2108`、`PLS-I8009`（設計包 §30.3 正式分配）；README 的設定與 report 格式章節；`sysinfo.snapshot` 日誌加上新的 skipped 計數。

驗收（測試）：

- 單元：每個新指標的換算與單位（以假的 sysinfo 值）；上限與優先順序；截斷後仍 ≤ 65536 bytes 且 ≤ 256 個（以最壞情況：16 個掛載點、8 個裝置、16 個介面、32 個感測器、label 值 256 bytes、`processes.command = "full"`）；送出的每一筆都通過 `validate_metrics`、`validate_process_list`、`validate_inventory`；`inventory_digest` 決定是否重送（相同不送、24 小時強制送、`invalid_report` 後不重送同一摘要）。
- 記憶體內的 v2 伺服器：協商與否 × `processes`／`inventory` 的四種組合，確認沒有協商時不送；舊版 Kairos（不宣告）時新指標照送、`processes` 與 `inventory` 不送。
- 緩衝：含 `processes` 的紀錄補送後與即時送出的內容相同；0.7 格式的舊紀錄可讀。
- 在 kp-build 上（Linux）實際執行一次、Windows 由 CI 的 Windows job 跑單元測試；Windows 虛擬服務帳號下的磁碟 I/O、行程、SMBIOS 需人工實測一次並記錄結果（第 4.5 節的〔待實測〕）。

### 16.2 Kairos 線

工作：

1. 協定處理（`kairos/src/connection.rs`）：依儲存後端宣告 `inventory`、`processes`；`inventory` 的處理（驗證、60 秒間隔、寫入、回應）；report 的 `processes` 驗證與寫入；移除本規格分支加上的「`inventory` 一律 `unknown_type`」。
2. 儲存（`kairos/src/store/sqlite.rs`）：schema v8（第 9 節）；計數器速率序列與重新啟動後的初始化；推算的使用率；每主機序列上限；資產的合併與變更歷史；行程快照與保留、容量刪除；`STORAGE.md` 更新（含第 10 節容量估算）。
3. Registry 與告警：`LastReport` 擴充；`metric_threshold` 的新 `metric`、掛載點／介面範圍、預覽；範本；（D5 採用時）主機重新開機事件；`ALERTS.md` 更新。
4. API（第 10 節）與 `API.md`；CSV 匯出欄位。
5. 主控台（`kairos/web`）：第 13 節（S05 概覽、趨勢、資產資訊、行程；S03 磁碟排行；S04 欄位與篩選；S09 指標選單），用語與單位依 copywriting；〔待設計〕的部分先以既有元件實作並標記，設計組補上後調整。
6. 代碼 `KRS-W2108`、`KRS-W4104`（設計包 §30.3 正式分配）。

驗收（測試）：

- 儲存：v7 → v8 migration（含既有資料）；計數器速率：正常、重置、uptime 變小、晚到、重新啟動後初始化；推算使用率只在缺 `usage` 時產生；序列上限；資產合併（省略欄位保留、陣列取代）與變更歷史的路徑；行程快照隨原始資料保留期限與容量刪除；durable 重複的 report 不寫出重複的快照。
- 協定整合測試（`integration_tests.rs`）：METRICS.md「範例」的每一則訊息經過真實連線得到範例中的回應；`inventory` 的 60 秒限制、`invalid_report`、未協商時 `unknown_type`；JSONL 後端不宣告兩個 feature。
- API：各端點的權限（`hosts:read`、群組範圍 404）、參數錯誤、`transform`；`e2e-rbac.mjs` 加入新端點。
- 告警：`disk` 每個掛載點各自觸發與恢復、`mountpoints` 範圍、網路速率的單位換算、預覽的「沒有回報」計數。
- 主控台：Playwright／axe（沿用 `kairos/web/tests`）涵蓋資產分頁、新圖表的表格檢視與鍵盤操作、空狀態文案。

### 16.3 端對端（`tests/e2e`，兩線完成後）

以實際的執行檔：0.8.0 × 0.8.0（全部功能：新指標、行程、資產、變更歷史、磁碟告警觸發）；0.8.0 Pulse × 以 0.7.0 規則運作的 Kairos（以送出 0.7.0 `hello_ack` 的假伺服器：不送 `processes`／`inventory`、新指標通過舊規則）；0.7.0 Pulse 的訊息 × 0.8.0 Kairos（推算的使用率、網路速率、資產分頁的空狀態）。

## 17. 待決事項（建議）

| # | 事項 | 建議 |
|---|---|---|
| D1 | 每台主機的儲存量約從 52 MB 增加到 129.1 MB（第 9.6 節），64 GiB 預設可完整保留的主機數從約 1,320 降到約 532 | **接受並更新估算與 S02 的公式**；不改預設的 `storage.max_total_bytes`（由管理者依規模調整，S02 會提示）。不建議降低 `report.interval` 的預設或少送指標：磁碟與網路是 MVP 的核心需求 |
| D2 | 資產（序號、MAC、IP）與行程（可能含命令列）是否需要獨立權限 | **沿用 `hosts:read`、不新增權限**（維持 35 個）：命令列預設不送，序號與網路位址對能看主機的人本來就是必要資訊。若之後要開放 `processes.command = "full"` 給大客戶，再新增 `hosts.processes:read`（只給管理者與維運者） |
| D3 | 行程的使用者名稱 | **本衝刺不送**（型別已預留 `user`）。需要時以自行實作的 uid／SID 查詢加入，不啟用 sysinfo 的 `user` feature |
| D4 | Linux 讀不到系統序號（root 才能讀） | **採用 `ExecStartPre=+` 複製到 `/run/pulse/`** 的作法（unit 改動小、Pulse 本身仍不需要特權）；不採用給 Pulse `CAP_DAC_READ_SEARCH`（權限過大） |
| D5 | 「主機重新開機」是否成為內建規則（31 條） | **加入**，資訊等級、不通知、預設啟用；設計組補文案與在事件列表中的呈現 |
| D6 | 資產自訂欄位（保管人、財產編號、位置）與編輯權限 | **延到下一個衝刺**，屆時一併決定是否新增 `hosts:write`（或 `hosts.assets:write`） |
| D7 | Pulse 本機緩衝的預設容量（每筆變大後，64 MiB 約保存 1～1.5 天） | **`buffer.max_mb` 預設改為 128**（約 2～3 天，與 `buffer.max_records` 50,000 筆的約 69 小時相近）；`buffer.max_age_hours` 168 不變 |
| D8 | 行程排行的間隔與數量 | 預設每 60 秒、各 5 個（本文件的預設）；保留 7 天，足以回答「昨晚 3 點是哪個程式」 |
| D9 | 0.2.0～0.7.0 Pulse 的磁碟使用率以推算值顯示（比 `df` 高約 5%） | **顯示並標示「推算」**，比不顯示有用；升級 Pulse 後自動改用精確值 |

## 18. 需要設計組補的項目

1. S05 的**行程**呈現（位置：概覽區塊或獨立分頁、欄位、取樣時間與 `?at=` 的歷史瀏覽、命令列的截斷與敏感資訊提示）。
2. S05 趨勢分頁的**磁碟讀寫、負載、置換空間**圖表，以及多個掛載點／介面時的呈現（小倍數或選擇器）。
3. S05「目前數值」卡的**置換空間、負載**，以及多個掛載點時「＋2」展開的互動。
4. S09 指標門檻的**新指標選單**（磁碟、網路、置換空間、每核心負載）與**掛載點／介面範圍**的輸入；預覽中「N 台沒有回報這個指標」的文案。
5. 舊版 Pulse 的**空狀態文案**（「這台主機的 Pulse 版本不回報此資訊（需要 0.8.0 以上）」）與「推算」標示的文案與 tooltip。
6. 「主機重新開機」事件（D5 採用時）的文案、等級與在 S05 時間軸的呈現。
7. 新指標與資產欄位的 **i18n key 與用語**（例：負載、置換空間、行程、開機時間、序號、BIOS、網卡）；`copywriting.md` §6 補上磁碟讀寫速率（MB/s，十進位）的寫法。
8. 第 14 節 4 個錯誤代碼的**正式分配**（已完成，設計包 §30.3）、標題與說明文案（§3、§4.2 範例）。
9. S02 的**容量估算公式**（每筆 report 約 530 bytes、每台約 129.1 MB，第 9.6 節）與原型 `mock-data.js` 的 `rawBytesPerReport`。
10. S04 **可選欄位**的最終清單（CPU、記憶體總量、廠牌型號、開機時間）與 OS 家族篩選。

## 範例

以下訊息是 `kairos-proto` 單元測試的測試向量（`src/metrics_spec_tests.rs`）：`P ` 開頭的行為 Pulse → Kairos、`K ` 開頭的行為 Kairos → Pulse，每一則都必須能解析、通過共用的檢查函式，並序列化回相同的 JSON。

### 協商與 report

0.8.0 的 Pulse 與 0.8.0 的 Kairos（SQLite）：hello 宣告 `inventory`、`processes`；report 帶新的標準指標（2 個掛載點、2 個磁碟 I/O 裝置、1 個介面、1 個溫度感測器）、`processes` 與 durable 欄位：

```
P {"type":"hello","seq":1,"proto":1,"proto_min":1,"proto_max":2,"features":["metrics","durable","inventory","processes"],"agent_id":"6f1c0e4a-1b2c-4d3e-9f00-112233445566","hostname":"web-01","agent_version":"0.8.0","heartbeat_interval_ms":15000,"ts":"2026-10-20T02:00:00.001Z","report_interval_ms":5000}
K {"type":"hello_ack","seq":1,"proto":2,"offline_after_ms":75000,"features":["metrics","durable","inventory","processes"],"server_time":"2026-10-20T02:00:00.009Z"}
P {"type":"report","seq":3,"ts":"2026-10-20T02:00:05.210Z","metrics":[{"name":"system.cpu.usage","value":17.84,"unit":"%"},{"name":"system.cpu.load_1m","value":2.15,"unit":"1"},{"name":"system.cpu.load_5m","value":1.98,"unit":"1"},{"name":"system.cpu.load_15m","value":1.75,"unit":"1"},{"name":"system.memory.total","value":51539607552,"unit":"By"},{"name":"system.memory.used","value":38833356800,"unit":"By"},{"name":"system.memory.available","value":12706250752,"unit":"By"},{"name":"system.swap.total","value":4294967296,"unit":"By"},{"name":"system.swap.used","value":0,"unit":"By"},{"name":"system.process.count","value":312,"unit":"{processes}"},{"name":"system.uptime","value":1532941,"unit":"s"},{"name":"system.filesystem.total","value":502468108288,"unit":"By","labels":{"fstype":"ext4","mountpoint":"/"}},{"name":"system.filesystem.available","value":211312345088,"unit":"By","labels":{"fstype":"ext4","mountpoint":"/"}},{"name":"system.filesystem.used","value":265601859584,"unit":"By","labels":{"fstype":"ext4","mountpoint":"/"}},{"name":"system.filesystem.usage","value":55.69,"unit":"%","labels":{"fstype":"ext4","mountpoint":"/"}},{"name":"system.filesystem.total","value":3998639460352,"unit":"By","labels":{"fstype":"xfs","mountpoint":"/data"}},{"name":"system.filesystem.available","value":1199591838106,"unit":"By","labels":{"fstype":"xfs","mountpoint":"/data"}},{"name":"system.filesystem.used","value":2799047622246,"unit":"By","labels":{"fstype":"xfs","mountpoint":"/data"}},{"name":"system.filesystem.usage","value":70.0,"unit":"%","labels":{"fstype":"xfs","mountpoint":"/data"}},{"name":"system.disk.read","value":912345678848,"unit":"By","labels":{"device":"nvme0n1p2"}},{"name":"system.disk.written","value":1534567890432,"unit":"By","labels":{"device":"nvme0n1p2"}},{"name":"system.disk.read_ops","value":23456789,"unit":"{operations}","labels":{"device":"nvme0n1p2"}},{"name":"system.disk.write_ops","value":45678901,"unit":"{operations}","labels":{"device":"nvme0n1p2"}},{"name":"system.disk.io_time","value":8123456,"unit":"ms","labels":{"device":"nvme0n1p2"}},{"name":"system.disk.read","value":4567890123,"unit":"By","labels":{"device":"sdb1"}},{"name":"system.disk.written","value":987654321098,"unit":"By","labels":{"device":"sdb1"}},{"name":"system.disk.read_ops","value":345678,"unit":"{operations}","labels":{"device":"sdb1"}},{"name":"system.disk.write_ops","value":9876543,"unit":"{operations}","labels":{"device":"sdb1"}},{"name":"system.disk.io_time","value":2345678,"unit":"ms","labels":{"device":"sdb1"}},{"name":"system.network.received","value":98765432101,"unit":"By","labels":{"device":"eth0"}},{"name":"system.network.transmitted","value":1234567890,"unit":"By","labels":{"device":"eth0"}},{"name":"system.network.packets_received","value":87654321,"unit":"{packets}","labels":{"device":"eth0"}},{"name":"system.network.packets_transmitted","value":7654321,"unit":"{packets}","labels":{"device":"eth0"}},{"name":"system.network.errors_received","value":0,"unit":"{errors}","labels":{"device":"eth0"}},{"name":"system.network.errors_transmitted","value":0,"unit":"{errors}","labels":{"device":"eth0"}},{"name":"system.temperature","value":48.5,"unit":"Cel","labels":{"sensor":"coretemp Package id 0"}}],"boot_id":"0b5e3c1d-7a2f-4e8b-9c6d-5f4a3b2c1d0e","report_seq":1201,"processes":{"top_cpu":[{"pid":2211,"name":"postgres","cpu_usage":9.41,"memory":4294967296},{"pid":1873,"name":"java","cpu_usage":3.2,"memory":8589934592}],"top_memory":[{"pid":1873,"name":"java","cpu_usage":3.2,"memory":8589934592},{"pid":2211,"name":"postgres","cpu_usage":9.41,"memory":4294967296}]}}
K {"type":"ack","seq":3}
```

### inventory

同一條連線，`hello_ack` 之後送出的資產（Linux，讀得到序號）：

```
P {"type":"inventory","seq":2,"ts":"2026-10-20T02:00:00.120Z","inventory":{"os":{"family":"linux","name":"Ubuntu","version":"24.04","long_version":"Ubuntu 24.04.1 LTS","kernel":"6.8.0-45-generic","arch":"x86_64"},"boot_time":"2026-10-02T08:11:04Z","cpu":{"model":"Intel(R) Xeon(R) Gold 6338 CPU @ 2.00GHz","vendor":"GenuineIntel","physical_cores":32,"logical_cores":64,"frequency_mhz":2000},"memory":{"total":51539607552,"swap_total":4294967296},"system":{"vendor":"Dell Inc.","product":"PowerEdge R650","serial":"7XK2LM3","board_vendor":"Dell Inc.","board_product":"0PJPW3","bios_vendor":"Dell Inc.","bios_version":"1.12.1"},"disks":[{"mountpoint":"/","device":"nvme0n1p2","fstype":"ext4","total":502468108288,"kind":"ssd","removable":false,"read_only":false},{"mountpoint":"/data","device":"sdb1","fstype":"xfs","total":3998639460352,"kind":"hdd","removable":false,"read_only":false}],"network_interfaces":[{"name":"eth0","mac":"52:54:00:12:34:56","addresses":["192.168.10.21/24","fe80::5054:ff:fe12:3456/64"],"mtu":1500}]}}
K {"type":"ack","seq":2}
```

### inventory（Windows）

虛擬機上的 Windows（沒有負載指標；`system` 只有廠牌與型號，序號為空字串所以不送）：

```
P {"type":"inventory","seq":2,"ts":"2026-10-20T02:00:00.300Z","inventory":{"os":{"family":"windows","name":"Windows","version":"11 (26200)","long_version":"Windows 11 Pro 24H2","kernel":"26200","arch":"x86_64"},"boot_time":"2026-10-19T23:58:12Z","cpu":{"model":"AMD EPYC 7763 64-Core Processor","vendor":"AuthenticAMD","physical_cores":4,"logical_cores":8,"frequency_mhz":2445},"memory":{"total":17179869184,"swap_total":2550136832},"system":{"vendor":"Microsoft Corporation","product":"Virtual Machine","bios_vendor":"Microsoft Corporation","bios_version":"Hyper-V UEFI Release v4.1"},"disks":[{"mountpoint":"C:\\","device":"C:","fstype":"NTFS","total":136365211648,"kind":"ssd","removable":false,"read_only":false}],"network_interfaces":[{"name":"Ethernet","mac":"00:15:5d:01:02:03","addresses":["10.0.0.15/24"],"mtu":1500}]}}
K {"type":"ack","seq":2}
```

### 錯誤

60 秒內再送一次 `inventory`；內容不合法的 `inventory`（介面的 MAC 格式錯誤）：

```
K {"type":"error","seq":4,"code":"storage_unavailable","message":"inventory rate limit exceeded","fatal":false}
K {"type":"error","seq":5,"code":"invalid_report","message":"network_interfaces eth0: mac must be 6 lowercase hex pairs separated by ':' and not all zero","fatal":false}
```
